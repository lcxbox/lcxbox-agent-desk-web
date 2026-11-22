//! YAML frontmatter of `SKILL.md`, read through the event parser so anchors,
//! aliases and extra documents are rejected instead of expanded.

use super::{
    MAX_COMPATIBILITY_CHARS, MAX_DESCRIPTION_CHARS, MAX_FRONTMATTER_BYTES, MAX_METADATA_ENTRIES,
    MAX_METADATA_KEY_CHARS, MAX_METADATA_VALUE_CHARS, PackageError, SkillPackage,
};
use std::collections::BTreeMap;
use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::TScalarStyle;

#[derive(Debug, Default)]
pub(super) struct Fields {
    pub name: String,
    pub description: String,
    pub compatibility: Option<String>,
    pub license: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub allowed_tools: Option<String>,
}

fn invalid(reason: &'static str) -> PackageError {
    PackageError::InvalidFrontmatter(reason)
}

/// Splits `SKILL.md` into validated fields and the instruction body.
pub(super) fn parse(skill_md: &str) -> Result<(Fields, String), PackageError> {
    let text = skill_md.strip_prefix('\u{feff}').unwrap_or(skill_md);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().ok_or(invalid("missing frontmatter"))?;
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return Err(invalid("missing frontmatter"));
    }
    let mut offset = first.len();
    let mut end = None;
    for line in lines {
        if line.trim_end_matches(['\r', '\n']).trim_end() == "---" {
            end = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let (header_end, body_start) = end.ok_or(invalid("unterminated frontmatter"))?;
    let header = &text[first.len()..header_end];
    if header.len() > MAX_FRONTMATTER_BYTES {
        return Err(invalid("frontmatter is too large"));
    }
    let fields = parse_fields(header)?;
    Ok((fields, text[body_start..].to_owned()))
}

struct Events<'a> {
    parser: Parser<std::str::Chars<'a>>,
}

impl Events<'_> {
    fn next(&mut self) -> Result<Event, PackageError> {
        let (event, _) = self
            .parser
            .next_token()
            .map_err(|_| invalid("malformed YAML"))?;
        match &event {
            Event::Alias(_) => Err(invalid("YAML aliases are not supported")),
            Event::Scalar(_, _, anchor, _)
            | Event::SequenceStart(anchor, _)
            | Event::MappingStart(anchor, _)
                if *anchor != 0 =>
            {
                Err(invalid("YAML anchors are not supported"))
            }
            _ => Ok(event),
        }
    }

    /// Consumes one complete value whose first event was already read.
    fn skip_value(&mut self, first: Event) -> Result<(), PackageError> {
        let mut depth = match first {
            Event::SequenceStart(..) | Event::MappingStart(..) => 1usize,
            Event::Scalar(..) => 0,
            _ => return Err(invalid("malformed YAML")),
        };
        while depth > 0 {
            match self.next()? {
                Event::SequenceStart(..) | Event::MappingStart(..) => depth += 1,
                Event::SequenceEnd | Event::MappingEnd => depth -= 1,
                Event::Scalar(..) => {}
                _ => return Err(invalid("malformed YAML")),
            }
        }
        Ok(())
    }
}

fn null_scalar(value: &str, style: TScalarStyle) -> bool {
    style == TScalarStyle::Plain && matches!(value, "" | "~" | "null" | "Null" | "NULL")
}

fn optional_scalar(
    event: Event,
    field: &'static str,
    max_chars: usize,
) -> Result<Option<String>, PackageError> {
    let Event::Scalar(value, style, ..) = event else {
        return Err(PackageError::InvalidField(field));
    };
    if null_scalar(&value, style) {
        return Ok(None);
    }
    if value.chars().count() > max_chars || value.trim().is_empty() {
        return Err(PackageError::InvalidField(field));
    }
    Ok(Some(value))
}

fn parse_fields(header: &str) -> Result<Fields, PackageError> {
    let mut events = Events {
        parser: Parser::new_from_str(header),
    };
    if events.next()? != Event::StreamStart || events.next()? != Event::DocumentStart {
        return Err(invalid("frontmatter must be a mapping"));
    }
    if !matches!(events.next()?, Event::MappingStart(..)) {
        return Err(invalid("frontmatter must be a mapping"));
    }
    let mut fields = Fields::default();
    let mut seen = std::collections::BTreeSet::new();
    let (mut name, mut description) = (None, None);
    loop {
        let key = match events.next()? {
            Event::MappingEnd => break,
            Event::Scalar(key, ..) => key,
            _ => return Err(invalid("frontmatter keys must be text")),
        };
        if !seen.insert(key.clone()) {
            return Err(invalid("duplicate frontmatter key"));
        }
        let value = events.next()?;
        match key.as_str() {
            "name" => {
                name = Some(
                    optional_scalar(value, "name", usize::MAX)
                        .ok()
                        .flatten()
                        .ok_or(PackageError::InvalidName)?,
                )
            }
            "description" => {
                description = optional_scalar(value, "description", MAX_DESCRIPTION_CHARS)?
            }
            "compatibility" => {
                fields.compatibility =
                    optional_scalar(value, "compatibility", MAX_COMPATIBILITY_CHARS)?
            }
            "license" => fields.license = optional_scalar(value, "license", 256)?,
            "allowed-tools" => fields.allowed_tools = allowed_tools(&mut events, value)?,
            "metadata" => fields.metadata = metadata(&mut events, value)?,
            _ => events.skip_value(value)?,
        }
    }
    if events.next()? != Event::DocumentEnd || events.next()? != Event::StreamEnd {
        return Err(invalid("frontmatter must be one YAML document"));
    }
    fields.name = name.ok_or(PackageError::InvalidName)?;
    fields.description = description.ok_or(PackageError::InvalidField("description"))?;
    Ok(fields)
}

/// The open standard uses one space-separated string; some clients write a list.
fn allowed_tools(events: &mut Events<'_>, first: Event) -> Result<Option<String>, PackageError> {
    const FIELD: &str = "allowed-tools";
    let value = match first {
        Event::SequenceStart(..) => {
            let mut tools = Vec::new();
            loop {
                match events.next()? {
                    Event::SequenceEnd => break,
                    Event::Scalar(tool, ..) if !tool.trim().is_empty() => tools.push(tool),
                    _ => return Err(PackageError::InvalidField(FIELD)),
                }
            }
            (!tools.is_empty()).then(|| tools.join(" "))
        }
        event => optional_scalar(event, FIELD, 4096)?,
    };
    match value {
        Some(value) if value.len() > 4096 => Err(PackageError::InvalidField(FIELD)),
        value => Ok(value),
    }
}

fn metadata(
    events: &mut Events<'_>,
    first: Event,
) -> Result<BTreeMap<String, String>, PackageError> {
    const FIELD: &str = "metadata";
    let mut map = BTreeMap::new();
    match first {
        Event::Scalar(value, style, ..) if null_scalar(&value, style) => return Ok(map),
        Event::MappingStart(..) => {}
        _ => return Err(PackageError::InvalidField(FIELD)),
    }
    loop {
        let key = match events.next()? {
            Event::MappingEnd => break,
            Event::Scalar(key, ..) => key,
            _ => return Err(PackageError::InvalidField(FIELD)),
        };
        let value = match events.next()? {
            Event::Scalar(value, ..) => value,
            other => {
                // Only string values are part of the standard; nested values are ignored.
                events.skip_value(other)?;
                continue;
            }
        };
        if key.is_empty()
            || key.chars().count() > MAX_METADATA_KEY_CHARS
            || value.chars().count() > MAX_METADATA_VALUE_CHARS
            || map.insert(key, value).is_some()
            || map.len() > MAX_METADATA_ENTRIES
        {
            return Err(PackageError::InvalidField(FIELD));
        }
    }
    Ok(map)
}

fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub(super) fn render(package: &SkillPackage) -> String {
    let mut out = format!(
        "---\nname: {}\ndescription: {}\n",
        package.name,
        quote(&package.description)
    );
    if let Some(value) = &package.compatibility {
        out.push_str(&format!("compatibility: {}\n", quote(value)));
    }
    if let Some(value) = &package.license {
        out.push_str(&format!("license: {}\n", quote(value)));
    }
    if let Some(value) = &package.allowed_tools {
        out.push_str(&format!("allowed-tools: {}\n", quote(value)));
    }
    if !package.metadata.is_empty() {
        out.push_str("metadata:\n");
        for (key, value) in &package.metadata {
            out.push_str(&format!("  {}: {}\n", quote(key), quote(value)));
        }
    }
    out.push_str("---\n");
    out.push_str(&package.body);
    out
}
