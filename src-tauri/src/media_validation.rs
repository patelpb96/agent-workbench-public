const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_PIXELS: u64 = 16_000_000;
const MAX_RANGE_BYTES: u64 = 8 * 1024 * 1024;

/// The host of a request for a selected background file. WebView2 presents the custom `background` scheme
/// either as `background.localhost` or, as the lifecycle log showed on this machine, as plain `localhost`.
/// Only the unguessable asset id in the path actually grants access; this is a sanity check on top of it.
pub(crate) fn media_host_ok(host: Option<&str>) -> bool {
    matches!(host, Some("background.localhost" | "localhost"))
}

pub(crate) fn image_dimensions_are_safe(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_IMAGE_DIMENSION
        && height <= MAX_IMAGE_DIMENSION
        && u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS
}

pub(crate) fn parse_byte_range(value: &str, length: u64) -> Result<(u64, u64), ()> {
    if length == 0 {
        return Err(());
    }
    let range = value.strip_prefix("bytes=").ok_or(())?;
    let (first, last) = range.split_once('-').ok_or(())?;
    let (start, requested_end) = if first.is_empty() {
        let suffix = last.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        (length.saturating_sub(suffix), length - 1)
    } else {
        let start = first.parse::<u64>().map_err(|_| ())?;
        if start >= length {
            return Err(());
        }
        let end = if last.is_empty() {
            length - 1
        } else {
            last.parse::<u64>().map_err(|_| ())?.min(length - 1)
        };
        (start, end)
    };
    if start > requested_end {
        return Err(());
    }
    let end = requested_end.min(start.saturating_add(MAX_RANGE_BYTES - 1));
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::{image_dimensions_are_safe, media_host_ok, parse_byte_range, MAX_RANGE_BYTES};

    #[test]
    fn the_background_scheme_host_is_accepted_in_both_forms_and_nothing_else() {
        assert!(media_host_ok(Some("background.localhost")));
        assert!(media_host_ok(Some("localhost")));
        for bad in [None, Some(""), Some("example.com"), Some("localhost.evil.com"), Some("evil-background.localhost"), Some("127.0.0.1")] {
            assert!(!media_host_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn allows_common_wallpaper_dimensions() {
        assert!(image_dimensions_are_safe(3840, 2160));
        assert!(image_dimensions_are_safe(4000, 4000));
        assert!(!image_dimensions_are_safe(4096, 4096));
    }

    #[test]
    fn rejects_zero_oversized_and_excessive_pixel_counts() {
        assert!(!image_dimensions_are_safe(0, 1));
        assert!(!image_dimensions_are_safe(16_385, 1));
        assert!(!image_dimensions_are_safe(5000, 4000));
    }

    #[test]
    fn parses_and_bounds_byte_ranges() {
        assert_eq!(parse_byte_range("bytes=5-10", 100), Ok((5, 10)));
        assert_eq!(parse_byte_range("bytes=8-", 100), Ok((8, 99)));
        assert_eq!(parse_byte_range("bytes=-8", 100), Ok((92, 99)));
        assert_eq!(
            parse_byte_range("bytes=0-99999999", 100_000_000),
            Ok((0, MAX_RANGE_BYTES - 1))
        );
    }

    #[test]
    fn rejects_unsatisfiable_and_malformed_byte_ranges() {
        assert!(parse_byte_range("bytes=100-", 100).is_err());
        assert!(parse_byte_range("bytes=9-2", 100).is_err());
        assert!(parse_byte_range("bytes=-0", 100).is_err());
        assert!(parse_byte_range("bytes=0-1,4-5", 100).is_err());
        assert!(parse_byte_range("bytes=0-1", 0).is_err());
    }
}
