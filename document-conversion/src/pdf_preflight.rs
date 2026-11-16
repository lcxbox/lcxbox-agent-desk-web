//! Cheap checks before a PDF reaches the parser. The parser itself is bounded
//! by the sandbox child's resource limits, not by byte-level emulation here.
use crate::{ConversionError, MAX_SOURCE_BYTES};

pub(crate) fn validate(source: &[u8]) -> Result<(), ConversionError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(ConversionError::new(
            "source_too_large",
            format!(
                "source is {} bytes, exceeding the {MAX_SOURCE_BYTES} byte limit",
                source.len()
            ),
        ));
    }
    if !source
        .get(..source.len().min(1_024))
        .is_some_and(|prefix| prefix.windows(5).any(|window| window == b"%PDF-"))
    {
        return Err(ConversionError::new(
            "source_format_mismatch",
            "source does not contain a PDF header",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_header_and_size_are_checked() {
        assert_eq!(
            validate(b"not a pdf").unwrap_err().code,
            "source_format_mismatch"
        );
        // Encryption, filters and indirect lengths are left to the parser.
        assert!(validate(b"%PDF-1.7\ntrailer << /Encrypt 2 0 R >>\n%%EOF").is_ok());
        assert!(validate(b"%PDF-1.7\n1 0 obj << /Length 2 0 R /Filter [/FlateDecode] >>").is_ok());
        let mut large = b"%PDF-1.7\n".to_vec();
        large.resize(MAX_SOURCE_BYTES + 1, b' ');
        assert_eq!(validate(&large).unwrap_err().code, "source_too_large");
    }
}
