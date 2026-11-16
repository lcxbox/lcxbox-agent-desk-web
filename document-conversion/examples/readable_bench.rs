//! Release benchmark for adversarial HTML inputs.
//!
//! `cargo run --release --example readable_bench -- <case>` converts one input
//! and prints the elapsed time and the heap peak during conversion; run each
//! case in its own process under `/usr/bin/time -l` (macOS) or
//! `/usr/bin/time -v` (Linux) to read the peak RSS. Without arguments it
//! lists the cases.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use desk_document_conversion::readable::{ReadableFormat, to_markdown};

const TWO_MIB: usize = 2 * 1024 * 1024;

/// Tracks live and peak heap bytes.
struct CountingAllocator;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let live = LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
        // SAFETY: forwards the caller's layout to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` was allocated by `alloc` above with the same layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn repeat_to(unit: &str, bytes: usize) -> String {
    unit.repeat(bytes / unit.len())
}

/// Many short lines spread over small paragraphs, so neither the sibling
/// limit nor a single text node bounds the line count.
const LINES: &str = "<p>a<br>a<br>a<br>a<br>a<br>a<br>a<br>a<br></p>";

/// Nesting for the amplification cases: just under the depth limit
/// (`NESTING`, default 250 for a 256 limit).
fn nesting() -> usize {
    std::env::var("NESTING")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(250)
}

fn input(case: &str) -> Option<String> {
    Some(match case {
        "baseline" => String::new(),
        "nested-div" => repeat_to("<div>", TWO_MIB),
        "br" => repeat_to("<br>", TWO_MIB),
        "self-closing-div" => "<div/>".repeat(20_000),
        "svg-style-div" => format!("<svg><style>{}", "<div>".repeat(20_000)),
        "attr-script-div" => format!("<div title=\"<script>\">{}", "<div>".repeat(20_000)),
        "bogus-comment-div" => format!("<!-->{}", "<div>".repeat(20_000)),
        "misnested-b-div" => "<b><div></b>".repeat(25_000),
        "formatting-500" => {
            let formatting: String = (0..500).map(|i| format!("<b a{i}>")).collect();
            format!("{formatting}{}", "x<p>".repeat(20_000))
        }
        "formatting-groups" => {
            let mut html = String::new();
            for group in 0..40 {
                for i in 0..300 {
                    html.push_str(&format!("<b g{group}a{i}>"));
                }
                html.push_str("<p>");
                html.push_str(&"x<p>".repeat(200));
            }
            html
        }
        "select-options" => format!(
            "<select><selectedcontent></selectedcontent>{}",
            "<option selected>o".repeat(20_000)
        ),
        "adjacent-b" => format!("<p>{}</p>", "<b>x</b>".repeat(20_000)),
        "html-attrs" => (0..150_000).map(|i| format!("<html a{i}>")).collect(),
        "foster-table" => repeat_to("<table>y</table>", TWO_MIB),
        "nested-template" => repeat_to("<template>", TWO_MIB),
        "wide-table-cell" => format!(
            "<table><tr><th>h</th></tr><tr><td>{}</td></tr>{}</table>",
            "w".repeat(1024 * 1024),
            "<tr><td>a</td></tr>".repeat(50_000)
        ),
        "wide-table-row" => format!(
            "<table><tr><th>h</th>{}</tr>{}</table>",
            "<td>c</td>".repeat(40_000),
            "<tr><td>a</td></tr>".repeat(40_000)
        ),
        "nested-blockquote-lines" => {
            let nesting = nesting();
            format!(
                "{}{}",
                "<blockquote>".repeat(nesting),
                repeat_to(LINES, TWO_MIB - nesting * 12)
            )
        }
        "nested-list-lines" => {
            let nesting = nesting() / 2;
            format!(
                "{}{}",
                "<ul><li>".repeat(nesting),
                repeat_to(LINES, TWO_MIB - nesting * 8)
            )
        }
        "deep-text" => format!("{}{}", "<div>".repeat(250), "word ".repeat(400_000)),
        "realistic" => {
            let mut html =
                String::from("<html><head><title>Docs</title></head><body><nav>menu</nav><main>");
            while html.len() < TWO_MIB {
                html.push_str(
                    "<h2>Section</h2><p>Some <a href=\"/x\">linked</a> text with <b>bold</b> and \
                     <code>code</code>.</p><ul><li>item one</li><li>item two</li></ul>\
                     <table><tr><th>k</th><th>v</th></tr><tr><td>a</td><td>1</td></tr></table>\
                     <pre><code>let x = 1;\nlet y = 2;</code></pre>",
                );
            }
            html.push_str("</main></body></html>");
            html
        }
        _ => return None,
    })
}

const CASES: [&str; 21] = [
    "baseline",
    "nested-div",
    "br",
    "self-closing-div",
    "svg-style-div",
    "attr-script-div",
    "bogus-comment-div",
    "misnested-b-div",
    "formatting-500",
    "formatting-groups",
    "select-options",
    "adjacent-b",
    "html-attrs",
    "foster-table",
    "nested-template",
    "wide-table-cell",
    "wide-table-row",
    "nested-blockquote-lines",
    "nested-list-lines",
    "deep-text",
    "realistic",
];

fn main() {
    let Some(case) = std::env::args().nth(1) else {
        println!("{}", CASES.join("\n"));
        return;
    };
    let html = input(&case).expect("unknown case");
    let base = url::Url::parse("https://example.com/page").unwrap();
    PEAK_BYTES.store(LIVE_BYTES.load(Ordering::Relaxed), Ordering::Relaxed);
    let heap_before = LIVE_BYTES.load(Ordering::Relaxed);
    let started = Instant::now();
    let document = to_markdown(
        html.as_bytes(),
        ReadableFormat::Html,
        None,
        &base,
        false,
        std::env::var("MAX_OUTPUT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1024 * 1024),
    )
    .unwrap();
    let elapsed = started.elapsed();
    let heap_peak = PEAK_BYTES.load(Ordering::Relaxed) - heap_before;
    println!(
        "{case}: input {} B, {:.1} ms, heap peak {:.1} MiB, output {} B, truncated {}, notices {:?}",
        html.len(),
        elapsed.as_secs_f64() * 1000.0,
        heap_peak as f64 / (1024.0 * 1024.0),
        document.markdown.len(),
        document.truncated,
        document.notices
    );
}
