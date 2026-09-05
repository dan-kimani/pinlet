//! Note color palette: CSS classes and contrast helpers.

use crate::storage::NoteColor;

impl NoteColor {
    /// CSS class applied to note windows and palette swatches.
    pub fn css_class(&self) -> &'static str {
        match self {
            Self::Yellow => "pinlet-yellow",
            Self::Green => "pinlet-green",
            Self::Blue => "pinlet-blue",
            Self::Pink => "pinlet-pink",
            Self::Purple => "pinlet-purple",
            Self::Charcoal => "pinlet-charcoal",
            Self::Custom(_) => "pinlet-custom",
        }
    }
}

/// CSS for a custom hex note color (dynamic, per window).
pub fn css_for_custom(hex: &str) -> String {
    format!(
        ".pinlet-custom {{ background-color: {hex}; }} \
         window.pinlet-custom textview.pinlet-body text {{ color: {}; }}",
        foreground_for(hex)
    )
}

/// Dark or light foreground that stays readable on `bg_hex`.
pub fn foreground_for(bg_hex: &str) -> &'static str {
    match parse_hex(bg_hex) {
        Some((r, g, b))
            if 0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b) > 150.0 =>
        {
            "#1a1a1a"
        }
        _ => "#f5f5f5",
    }
}

/// Parse `#RRGGBB` into components.
fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    let digits = hex.strip_prefix('#')?;
    if digits.len() != 6 {
        return None;
    }
    Some((
        u8::from_str_radix(digits.get(0..2)?, 16).ok()?,
        u8::from_str_radix(digits.get(2..4)?, 16).ok()?,
        u8::from_str_radix(digits.get(4..6)?, 16).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_picks_dark_on_light_and_light_on_dark() {
        assert_eq!(foreground_for("#fbf3b0"), "#1a1a1a");
        assert_eq!(foreground_for("#3c3f42"), "#f5f5f5");
    }

    #[test]
    fn invalid_hex_falls_back_to_light_text() {
        assert_eq!(foreground_for("nonsense"), "#f5f5f5");
    }
}
