use std::time::{Duration, Instant};

use markup5ever_rcdom::{Handle, NodeData};

use super::*;
use crate::readable::{ReadableFormat, clamp_published_at};

fn base() -> url::Url {
    url::Url::parse("https://example.com/docs/page.html").unwrap()
}

fn convert(html: &str) -> ReadableDocument {
    to_markdown(html, &base())
}

fn run_on_small_stack<T: Send + 'static>(task: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(task)
        .unwrap()
        .join()
        .unwrap()
}

fn count_nodes(root: &Handle) -> usize {
    let mut count = 0;
    let mut stack = vec![root.clone()];
    while let Some(node) = stack.pop() {
        count += 1;
        stack.extend(node.children.borrow().iter().cloned());
    }
    count
}

fn div_node() -> Handle {
    markup5ever_rcdom::Node::new(NodeData::Element {
        name: html5ever::QualName::new(None, html5ever::ns!(html), html5ever::local_name!("div")),
        attrs: Default::default(),
        template_contents: Default::default(),
        mathml_annotation_xml_integration_point: false,
    })
}

// ---- structure ----

#[test]
fn renders_structure() {
    let document = convert(
        r#"<html><body>
        <h1>Title</h1><h2>Section</h2><h3>Sub</h3>
        <p>Intro <strong>bold</strong> and <em>soft</em>.</p>
        <ul><li>one</li><li>two<ul><li>nested</li></ul></li></ul>
        <ol><li>first</li><li>second</li></ol>
        <table><thead><tr><th>Name</th><th>Size</th></tr></thead>
        <tbody><tr><td>a</td><td>1</td></tr></tbody></table>
        <pre><code class="language-rust">fn main() {
    println!("hi");
}</code></pre>
        <p>See <a href="https://other.example/x">other</a>.</p>
        </body></html>"#,
    );
    let markdown = &document.markdown;
    assert!(markdown.contains("# Title\n"), "{markdown}");
    assert!(markdown.contains("## Section\n"), "{markdown}");
    assert!(markdown.contains("### Sub\n"), "{markdown}");
    assert!(
        markdown.contains("Intro **bold** and *soft*."),
        "{markdown}"
    );
    assert!(
        markdown.contains("*   one\n*   two\n    *   nested"),
        "{markdown}"
    );
    assert!(markdown.contains("1.  first\n2.  second"), "{markdown}");
    assert!(
        markdown.contains("| Name | Size |\n| --- | --- |\n| a | 1 |\n\n"),
        "{markdown}"
    );
    assert!(
        markdown.contains("```rust\nfn main() {\n    println!(\"hi\");\n}\n```"),
        "{markdown}"
    );
    assert!(
        markdown.contains("See [other](https://other.example/x)."),
        "{markdown}"
    );
    assert!(!markdown.contains("\n\n\n"), "{markdown}");
    assert!(!document.truncated);
}

#[test]
fn tables_render_without_padding_and_layout_tables_as_blocks() {
    let document = convert(
        "<table><caption>Sizes</caption><thead><tr><th>Name</th><th>Size | unit</th></tr></thead>\
         <tbody><tr><td>long value here</td><td>1</td></tr><tr><td>b</td></tr></tbody></table>",
    );
    assert_eq!(
        document.markdown,
        "Sizes\n| Name | Size &#124; unit |\n| --- | --- |\n| long value here | 1 |\n| b |"
    );

    let document = convert("<table><tr><td>layout</td><td>cells</td></tr></table>");
    assert_eq!(document.markdown, "layout\n\ncells");

    let document = convert(
        "<table><tr><th>outer</th></tr><tr><td><table><tr><th>inner</th></tr>\
         <tr><td>x</td></tr></table></td></tr></table>",
    );
    assert_eq!(document.markdown, "| outer |\n| --- |\n| inner x |");
}

#[test]
fn deep_quotes_and_list_items_are_flattened() {
    let document = convert("<blockquote><blockquote><p>quoted</p></blockquote></blockquote>");
    assert_eq!(document.markdown, "> > quoted");

    let html = format!("{}<p>deep</p>", "<blockquote>".repeat(40));
    let document = convert(&html);
    let prefix = "> ".repeat(MAX_QUOTE_LIST_NESTING);
    assert_eq!(document.markdown, format!("{prefix}deep"));
    assert_complete(&document);

    let html = format!("{}<p>item</p>", "<ul><li>".repeat(40));
    let document = convert(&html);
    let line = document
        .markdown
        .lines()
        .find(|line| line.contains("item"))
        .unwrap();
    let indent = line.len() - line.trim_start().len();
    assert!(indent <= 4 * MAX_QUOTE_LIST_NESTING, "{indent}");
}

#[test]
fn links_become_absolute() {
    let document = convert(
        r##"<body><p><a href="../guide/intro.html">rel</a> <a href="/root?q=1">root</a>
        <a href="//cdn.example.org/x">proto</a> <a href="#top">frag</a></p></body>"##,
    );
    let markdown = &document.markdown;
    assert!(
        markdown.contains("[rel](https://example.com/guide/intro.html)"),
        "{markdown}"
    );
    assert!(
        markdown.contains("[root](https://example.com/root?q=1)"),
        "{markdown}"
    );
    assert!(
        markdown.contains("[proto](https://cdn.example.org/x)"),
        "{markdown}"
    );
    assert!(
        markdown.contains("[frag](https://example.com/docs/page.html#top)"),
        "{markdown}"
    );
}

#[test]
fn base_href_is_used_for_links() {
    let document = convert(
        r#"<head><base href="https://static.example.net/v2/"></head>
        <body><p><a href="item">item</a></p></body>"#,
    );
    assert!(
        document
            .markdown
            .contains("[item](https://static.example.net/v2/item)"),
        "{}",
        document.markdown
    );

    let document = convert(
        r#"<head><base href="javascript:alert(1)"></head><body><p><a href="item">item</a></p></body>"#,
    );
    assert!(
        document
            .markdown
            .contains("[item](https://example.com/docs/item)"),
        "{}",
        document.markdown
    );
}

#[test]
fn non_http_links_keep_only_text() {
    let document = convert(
        r#"<body><p><a href="javascript:alert(1)">run</a> <a href="data:text/html,x">data</a>
        <a href="mailto:a@example.com">mail</a> <a>plain</a></p></body>"#,
    );
    let markdown = &document.markdown;
    assert!(markdown.contains("run data mail plain"), "{markdown}");
    assert!(!markdown.contains("javascript"), "{markdown}");
    assert!(!markdown.contains("data:"), "{markdown}");
    assert!(!markdown.contains("]("), "{markdown}");
}

#[test]
fn images_render_alt_text_only() {
    let document = convert(
        r#"<body><p>Before <img src="/a.png" alt="A diagram"> after <img src="/b.png"></p></body>"#,
    );
    assert_eq!(document.markdown, "Before A diagram after");
}

#[test]
fn skipped_tags_are_removed() {
    let document = convert(
        r#"<body><nav>menu</nav><aside>ads</aside><script>var s;</script><style>p{}</style>
        <noscript>enable js</noscript><template><p>tpl</p></template><svg><text>vec</text></svg>
        <canvas>cv</canvas><iframe>fr</iframe><object>ob</object><embed>
        <form><p>Kept form text</p><input value="in"><button>btn</button>
        <select><option>opt</option></select><textarea>ta</textarea></form>
        <p>Body text</p></body>"#,
    );
    assert_eq!(document.markdown, "Kept form text\n\nBody text");
}

#[test]
fn root_prefers_main_then_single_article_then_body() {
    let document = convert(
        "<body><p>outside</p><article>a1</article><main><p>main text</p></main><main>second</main></body>",
    );
    assert_eq!(document.markdown, "main text");

    let document = convert("<body><p>outside</p><article><p>only article</p></article></body>");
    assert_eq!(document.markdown, "only article");

    let document =
        convert("<body><p>outside</p><article>one</article><article>two</article></body>");
    assert_eq!(document.markdown, "outside\n\none\n\ntwo");
}

#[test]
fn page_header_and_footer_are_dropped_but_kept_inside_article() {
    let document = convert(
        "<body><header>site header</header><div><header>inner header</header><p>text</p></div>\
         <footer>site footer</footer></body>",
    );
    assert_eq!(document.markdown, "inner header\n\ntext");

    let document = convert(
        "<body><header>site</header><article><header>Article header</header>\
         <p>text</p><footer>Article footer</footer></article><footer>site</footer></body>",
    );
    assert_eq!(
        document.markdown,
        "Article header\n\ntext\n\nArticle footer"
    );
}

#[test]
fn form_wrapping_the_page_keeps_the_body() {
    let document = convert(
        r#"<body><form action="/postback"><div><h1>Heading</h1><p>Content inside a page-wide form.</p>
        <input type="hidden" value="state"></div></form></body>"#,
    );
    assert_eq!(
        document.markdown,
        "# Heading\n\nContent inside a page-wide form."
    );
}

#[test]
fn title_and_published_time_are_extracted() {
    let document = convert(
        r#"<head><title>  Page
        Title </title><meta property="article:published_time" content=" 2026-11-15T08:00:00Z ">
        </head><body><svg><title>icon</title></svg><p>x</p></body>"#,
    );
    assert_eq!(document.title.as_deref(), Some("Page Title"));
    assert_eq!(
        document.published_at.as_deref(),
        Some("2026-11-15T08:00:00Z")
    );

    let document = convert(r#"<head><meta itemprop="datePublished" content="2026-01-02"></head>"#);
    assert_eq!(document.published_at.as_deref(), Some("2026-01-02"));
    assert_eq!(document.title, None);
}

#[test]
fn title_and_published_time_are_clamped() {
    let long_title = "t".repeat(5_000);
    let long_date = "2".repeat(300);
    let document = convert(&format!(
        r#"<head><title>{long_title}</title><meta name="date" content="{long_date}"></head><body>x</body>"#
    ));
    let title = document.title.unwrap();
    assert_eq!(title.chars().count(), 2_000);
    assert_eq!(document.published_at, None);

    let document =
        convert("<head><meta name=\"pubdate\" content=\"2026-11-15\u{1}\"><title> </title></head>");
    assert_eq!(document.published_at, None);
    assert_eq!(document.title, None);

    // Values that survive clamping pass the fetch result validator's rules.
    let document = convert(&format!(
        "<head><title>{}</title><meta name=date content=\"{}\"></head>",
        "标题".repeat(1_500),
        "年".repeat(85)
    ));
    let title = document.title.unwrap();
    assert!(title.chars().count() <= 2_000);
    let published_at = document.published_at.unwrap();
    assert!(published_at.len() <= 256 && !published_at.chars().any(char::is_control));
    assert_eq!(clamp_published_at(&published_at), Some(published_at));
}

#[test]
fn entry_point_decodes_and_converts_html() {
    let mut source =
        br#"<html><head><meta charset="gbk"><title>t</title></head><body><p>"#.to_vec();
    source.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4]);
    let document =
        crate::readable::to_markdown(&source, ReadableFormat::Html, None, &base(), false, 1024)
            .unwrap();
    assert_eq!(document.markdown, "中文");
    assert_eq!(document.title.as_deref(), Some("t"));
}

// ---- regression: ordinary pages must not hit any budget ----

fn assert_complete(document: &ReadableDocument) {
    assert!(!document.truncated, "{:?}", document.notices);
    assert!(document.notices.is_empty(), "{:?}", document.notices);
}

#[test]
fn many_sibling_divs_are_not_truncated() {
    let html = (0..1_000)
        .map(|i| format!("<div>d{i}</div>"))
        .collect::<String>();
    let document = convert(&html);
    assert_complete(&document);
    assert!(document.markdown.contains("d0") && document.markdown.contains("d999"));
}

#[test]
fn many_paragraphs_with_spans_are_not_truncated() {
    let html = "<p><span>a</span></p>".repeat(1_000);
    let document = convert(&html);
    assert_complete(&document);
    assert_eq!(document.markdown.matches('a').count(), 1_000);
}

#[test]
fn entity_dense_paragraph_is_not_truncated() {
    let html = format!("<p>{}</p>", "a&amp;b ".repeat(50_000));
    let (dom, budget) = parse(&html);
    assert!(budget.nodes() < 100, "{}", budget.nodes());
    drop(dom);
    let document = convert(&html);
    assert_complete(&document);
    assert_eq!(document.markdown.matches("a&b").count(), 50_000);
}

#[test]
fn long_preformatted_block_is_not_truncated() {
    let html = format!("<pre>{}</pre>", "line &lt;x&gt;\n".repeat(50_000));
    let document = convert(&html);
    assert_complete(&document);
    assert_eq!(document.markdown.matches("line <x>").count(), 50_000);
}

// ---- adversarial inputs ----

const TWO_MIB: usize = 2 * 1024 * 1024;

fn time_limit() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(3)
    }
}

/// Parses and converts `html`, asserting the tree stays within the node
/// budget and the conversion finishes in time.
fn convert_adversarial(html: &str) -> ReadableDocument {
    let (dom, budget) = parse(html);
    let nodes = count_nodes(&dom.document);
    assert!(nodes <= MAX_HTML_NODES + 2, "tree has {nodes} nodes");
    assert!(budget.nodes() <= MAX_HTML_NODES * 3, "{}", budget.nodes());
    drop(dom);

    let started = Instant::now();
    let document = convert(html);
    let elapsed = started.elapsed();
    assert!(elapsed < time_limit(), "conversion took {elapsed:?}");
    document
}

fn repeat_to(unit: &str, bytes: usize) -> String {
    unit.repeat(bytes / unit.len())
}

#[test]
fn adversarial_nested_divs() {
    let html = format!("{}x", repeat_to("<div>", TWO_MIB));
    let document = run_on_small_stack(move || convert_adversarial(&html));
    assert!(document.truncated);
    assert!(
        document
            .notices
            .contains(&ReadableNotice::ParseBudgetExceeded)
    );
}

#[test]
fn adversarial_line_breaks() {
    let html = format!("<p>x{}", repeat_to("<br>", TWO_MIB));
    let document = convert_adversarial(&html);
    assert!(document.truncated);
    assert!(document.markdown.starts_with('x'));
}

#[test]
fn adversarial_self_closing_divs() {
    let html = format!("<p>x</p>{}", "<div/>".repeat(20_000));
    let document = convert_adversarial(&html);
    assert_eq!(document.markdown, "x");
}

#[test]
fn adversarial_svg_style_then_divs() {
    let html = format!("<p>x</p><svg><style>{}", "<div>".repeat(20_000));
    let document = convert_adversarial(&html);
    assert!(document.markdown.starts_with('x'));
}

#[test]
fn adversarial_attribute_with_script_then_divs() {
    let html = format!(
        "<p>x</p><div title=\"<script>\">{}y",
        "<div>".repeat(20_000)
    );
    let document = convert_adversarial(&html);
    assert!(document.markdown.starts_with('x'));
    assert!(document.truncated);
}

#[test]
fn adversarial_bogus_comment_then_divs() {
    let html = format!("<p>x</p><!-->{}y", "<div>".repeat(20_000));
    let document = convert_adversarial(&html);
    assert!(document.markdown.starts_with('x'));
}

#[test]
fn adversarial_misnested_formatting() {
    let html = format!("<p>x</p>{}", "<b><div></b>".repeat(25_000));
    let document = convert_adversarial(&html);
    assert!(document.markdown.starts_with('x'));
}

#[test]
fn adversarial_formatting_reconstruction() {
    let formatting: String = (0..500).map(|i| format!("<b a{i}>")).collect();
    let html = format!("<p>first</p>{formatting}{}", "x<p>".repeat(20_000));
    let document = convert_adversarial(&html);
    assert!(document.truncated);
    assert!(document.markdown.starts_with("first"));
}

#[test]
fn adversarial_formatting_reconstruction_across_chunks() {
    // Groups shallower than the depth limit so the node budget is what stops
    // the reconstruction amplification.
    let group_size = MAX_HTML_DEPTH / 2;
    let mut html = String::new();
    for group in 0..40 {
        for i in 0..group_size {
            html.push_str(&format!("<b g{group}a{i}>"));
        }
        html.push_str("<p>");
        html.push_str(&"x<p>".repeat(200));
    }
    let document = convert_adversarial(&html);
    assert!(document.truncated);
    assert!(
        document
            .notices
            .contains(&ReadableNotice::ParseBudgetExceeded)
    );
    assert!(document.markdown.contains('x'));
}

#[test]
fn adversarial_select_options() {
    let html = format!(
        "<p>x</p><select><selectedcontent></selectedcontent>{}",
        "<option selected>o".repeat(20_000)
    );
    let document = convert_adversarial(&html);
    assert_eq!(document.markdown, "x");
}

#[test]
fn adversarial_adjacent_inline_elements() {
    let html = format!("<p>{}</p>", "<b>x</b>".repeat(20_000));
    let document = convert_adversarial(&html);
    assert_eq!(
        document.markdown,
        format!("**{}**", "x".repeat(MAX_HTML_SIBLINGS))
    );
    assert_eq!(document.notices, vec![ReadableNotice::SiblingsTruncated]);
}

#[test]
fn adversarial_adjacent_inline_elements_with_comments() {
    let html = format!("<p>{}</p>", "<font><!---->x</font>".repeat(5_000));
    let document = convert_adversarial(&html);
    assert_eq!(document.markdown, "x".repeat(5_000));
    assert_complete(&document);
}

/// Many short lines in small paragraphs: neither the sibling limit nor a
/// single text node bounds the line count.
const LINES: &str = "<p>a<br>a<br>a<br>a<br>a<br>a<br>a<br>a<br></p>";

#[test]
fn adversarial_nested_blockquote_lines() {
    let html = format!(
        "{}{}",
        "<blockquote>".repeat(MAX_HTML_DEPTH - 6),
        repeat_to(LINES, TWO_MIB)
    );
    let document = convert_adversarial(&html);
    let prefix = "> ".repeat(MAX_QUOTE_LIST_NESTING);
    assert!(document.markdown.starts_with(&format!("{prefix}a")));
    assert!(!document.markdown.contains(&format!("{prefix}> ")));
}

#[test]
fn adversarial_nested_list_lines() {
    let html = format!(
        "{}{}",
        "<ul><li>".repeat(MAX_HTML_DEPTH / 2 - 4),
        repeat_to(LINES, TWO_MIB)
    );
    let document = convert_adversarial(&html);
    let widest = document
        .markdown
        .lines()
        .map(|line| line.len() - line.trim_start().len())
        .max()
        .unwrap();
    assert!(widest <= 4 * MAX_QUOTE_LIST_NESTING, "{widest}");
}

#[test]
fn adversarial_wide_table_cell_is_not_padded() {
    let html = format!(
        "<table><tr><th>h</th></tr><tr><td>{}</td></tr>{}</table>",
        "w".repeat(1024 * 1024),
        "<tr><td>a</td></tr>".repeat(30_000)
    );
    let document = convert_adversarial(&html);
    assert!(document.markdown.starts_with("| h |\n| --- |\n| www"));
    assert!(document.markdown.ends_with("| a |"));
    assert!(document.markdown.len() < 2 * 1024 * 1024);
}

#[test]
fn adversarial_repeated_html_attributes() {
    let html: String = (0..150_000).map(|i| format!("<html a{i}>")).collect();
    let html = format!("<p>x</p>{html}");
    let document = convert_adversarial(&html);
    assert!(document.truncated);
    assert!(document.markdown.starts_with('x'));
}

#[test]
fn adversarial_foster_parented_text() {
    let html = format!("<p>x</p>{}", repeat_to("<table>y</table>", TWO_MIB));
    let document = convert_adversarial(&html);
    assert!(document.markdown.starts_with('x'));
}

#[test]
fn adversarial_nested_templates() {
    let html = format!("<p>x</p>{}", repeat_to("<template>", TWO_MIB));
    let document = run_on_small_stack(move || convert_adversarial(&html));
    assert!(document.markdown.starts_with('x'));
}

#[test]
fn adversarial_many_siblings_are_truncated() {
    let html = format!(
        "<div>{}</div><p>after</p>",
        "<i>a</i><b>b</b>".repeat(MAX_HTML_SIBLINGS)
    );
    let document = convert_adversarial(&html);
    assert!(document.truncated);
    assert_eq!(document.notices, vec![ReadableNotice::SiblingsTruncated]);
    assert!(document.markdown.ends_with("after"));
}

#[test]
fn deepest_admitted_input_converts_and_drops_on_small_stack() {
    // `<html>` and `<body>` take two levels; the text sits at the depth limit.
    let html = format!("{}deep", "<div>".repeat(MAX_HTML_DEPTH - 3));
    let document = run_on_small_stack(move || convert(&html));
    assert_eq!(document.markdown, "deep");
    assert_complete(&document);

    let html = format!("{}deep", "<div>".repeat(MAX_HTML_DEPTH - 1));
    let document = run_on_small_stack(move || convert(&html));
    assert!(document.truncated);
}

#[test]
fn reparented_deep_trees_are_pruned_on_small_stack() {
    let document = run_on_small_stack(|| {
        let (dom, _) = parse("<body><p>top</p></body>");
        // Builds a chain far deeper than the parse-time check admits, as
        // tree-builder reparenting could.
        let body = clean::clean(&dom.document, &base()).root;
        let mut parent = body;
        for _ in 0..200_000 {
            let child = div_node();
            child.parent.set(Some(std::rc::Rc::downgrade(&parent)));
            parent.children.borrow_mut().push(child.clone());
            parent = child;
        }
        drop(parent);
        let cleaned = clean::clean(&dom.document, &base());
        assert!(cleaned.depth_pruned);
        htmd::HtmlToMarkdown::new().tree_to_markdown(&cleaned.root)
    });
    assert_eq!(document, "top");
}

#[test]
fn deep_unpruned_tree_drops_on_small_stack() {
    run_on_small_stack(|| {
        let (dom, _) = parse("<body></body>");
        let mut parent = dom.document.clone();
        for _ in 0..200_000 {
            let child = div_node();
            parent.children.borrow_mut().push(child.clone());
            parent = child;
        }
        drop(parent);
        drop(dom);
    });
}
