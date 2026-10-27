//! Unmodified identities already shipped by the four native applications.
//! Decode/cache once in each toolkit; these are not the JPEG's original assets.

pub const PRODUCT_ICON_SIZE: f32 = 24.0;
pub const SPACE_ICON_SIZE: f32 = 22.0;

pub fn png(product: &str) -> Option<&'static [u8]> {
    if product.eq_ignore_ascii_case("anvil") {
        Some(include_bytes!("../assets/identities/anvil.png"))
    } else if product.eq_ignore_ascii_case("forge") {
        Some(include_bytes!("../assets/identities/forge.png"))
    } else if product.eq_ignore_ascii_case("ember") {
        Some(include_bytes!("../assets/identities/ember.png"))
    } else if product.eq_ignore_ascii_case("frost") {
        Some(include_bytes!("../assets/identities/frost.png"))
    } else {
        None
    }
}
