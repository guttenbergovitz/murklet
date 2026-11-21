//! HTTP `Accept` header parsing and output format negotiation.

use crate::domain::OutputFormat;

/// Selects the best supported v0.1 [`OutputFormat`] from an HTTP `Accept` header.
///
/// Evaluates `q` quality weights (`0.0..=1.0`, defaulting to `1.0`) across the
/// negotiable formats supported by the v0.1 encoder (`image/webp`, `image/jpeg`,
/// `image/png`), breaking ties in favour of `Webp > Jpeg > Png`. Falls back to
/// [`OutputFormat::Jpeg`] when no explicit match is found or the header is absent.
#[must_use]
pub fn negotiate_format(accept_header: Option<&str>) -> OutputFormat {
    let Some(raw) = accept_header else {
        return OutputFormat::Jpeg;
    };

    let mut webp_q: Option<u16> = None;
    let mut jpeg_q: Option<u16> = None;
    let mut png_q: Option<u16> = None;

    for item in raw.split(',') {
        let mut parts = item.split(';');
        let Some(media_range) = parts.next().map(str::trim) else {
            continue;
        };
        if media_range.is_empty() {
            continue;
        }

        let Some(q) = parse_quality_param(parts) else {
            continue;
        };

        if media_range.eq_ignore_ascii_case("image/webp") {
            webp_q = Some(webp_q.map_or(q, |prev| prev.max(q)));
        } else if media_range.eq_ignore_ascii_case("image/jpeg") {
            jpeg_q = Some(jpeg_q.map_or(q, |prev| prev.max(q)));
        } else if media_range.eq_ignore_ascii_case("image/png") {
            png_q = Some(png_q.map_or(q, |prev| prev.max(q)));
        }
    }

    // Compare in preference order (WebP > JPEG > PNG) so ties pick the more modern format.
    let candidates = [
        (OutputFormat::Webp, webp_q.unwrap_or(0)),
        (OutputFormat::Jpeg, jpeg_q.unwrap_or(0)),
        (OutputFormat::Png, png_q.unwrap_or(0)),
    ];

    let mut best_format = OutputFormat::Jpeg;
    let mut best_q: u16 = 0;

    for (format, q) in candidates {
        if q > best_q {
            best_q = q;
            best_format = format;
        }
    }

    best_format
}

/// Parses an optional `q=` parameter into milli-units (`0..=1000`), returning `None`
/// if the parameter is malformed.
fn parse_quality_param<'a>(params: impl Iterator<Item = &'a str>) -> Option<u16> {
    let mut q_value: u16 = 1000;

    for param in params {
        let trimmed = param.trim();
        if let Some((key, val)) = trimmed.split_once('=')
            && key.trim().eq_ignore_ascii_case("q")
        {
            q_value = parse_q_millis(val.trim())?;
        }
    }

    Some(q_value)
}

/// Parses an RFC 9110 qvalue (`0`, `1`, `0.xxx`, `1.000`) into `0..=1000` without floats.
fn parse_q_millis(raw: &str) -> Option<u16> {
    if raw == "1" || raw == "1.0" || raw == "1.00" || raw == "1.000" {
        return Some(1000);
    }
    if raw == "0" {
        return Some(0);
    }
    let frac = raw.strip_prefix("0.")?;
    if frac.is_empty() || frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let mut millis: u16 = 0;
    let mut place: u16 = 100;
    for b in frac.bytes() {
        millis += u16::from(b - b'0') * place;
        place /= 10;
    }
    Some(millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_webp_for_standard_browser_accept_headers() {
        let chrome = "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8";
        assert_eq!(negotiate_format(Some(chrome)), OutputFormat::Webp);

        let firefox = "image/avif,image/webp,image/png,image/svg+xml,image/*;q=0.8,*/*;q=0.5";
        assert_eq!(negotiate_format(Some(firefox)), OutputFormat::Webp);
    }

    #[test]
    fn respects_quality_weights_and_exclusions() {
        assert_eq!(
            negotiate_format(Some("image/webp;q=0.5, image/jpeg;q=0.9")),
            OutputFormat::Jpeg
        );
        assert_eq!(
            negotiate_format(Some("image/webp;q=0, image/png;q=0.8")),
            OutputFormat::Png
        );
        assert_eq!(
            negotiate_format(Some("image/webp;q=0, image/jpeg;q=0, image/png;q=0")),
            OutputFormat::Jpeg
        );
    }

    #[test]
    fn falls_back_to_jpeg_for_missing_wildcard_or_unrecognised_headers() {
        assert_eq!(negotiate_format(None), OutputFormat::Jpeg);
        assert_eq!(negotiate_format(Some("*/*")), OutputFormat::Jpeg);
        assert_eq!(negotiate_format(Some("image/avif")), OutputFormat::Jpeg);
        assert_eq!(
            negotiate_format(Some("image/webp;q=invalid, image/png")),
            OutputFormat::Png
        );
    }
}
