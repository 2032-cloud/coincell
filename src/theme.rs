//! Turning the `GET /api/branding` palette into egui visuals, and deciding which
//! scheme (light or dark) to show.
//!
//! The colours used to be hard-coded here; now they come from [`api::Branding`],
//! with a copy baked into the binary (`assets/branding.json`) as the offline
//! fallback. `App` refreshes the branding at launch, then calls [`apply`] with
//! the scheme [`resolve`] picks from the user's `[appearance].theme`, the account
//! setting, and the OS.
//!
//! Typography from the payload is deliberately not applied: `font_source_url` is
//! `None` (nothing to download) and egui can't resolve a CSS family by name, the
//! bundled font stands in. Wire it up here if the service starts serving a font.

use eframe::egui::{self, Color32};

use crate::api::{BrandPalette, Branding};
use crate::config::Theme;

/// The concrete choice after all preferences are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Light,
    Dark,
}

impl Scheme {
    fn egui(self) -> egui::Theme {
        match self {
            Scheme::Light => egui::Theme::Light,
            Scheme::Dark => egui::Theme::Dark,
        }
    }
}

impl From<egui::Theme> for Scheme {
    fn from(t: egui::Theme) -> Self {
        match t {
            egui::Theme::Light => Scheme::Light,
            egui::Theme::Dark => Scheme::Dark,
        }
    }
}

/// The branding copy compiled into the binary. Used until (and whenever) the
/// live `GET /api/branding` fetch fails.
pub fn baked() -> Branding {
    const JSON: &str = include_str!("../assets/branding.json");
    serde_json::from_str(JSON).expect("baked assets/branding.json must deserialize into api::Branding")
}

/// Decide light vs dark from the user's preference, the account's setting
/// (`GET /api/me`'s `theme`: `Some(true)` = light, `Some(false)` = dark, `None` =
/// follow system), the OS theme, and finally the branding's `default_scheme`.
pub fn resolve(pref: Theme, account_theme: Option<Option<bool>>, system: Option<egui::Theme>, branding: &Branding) -> Scheme {
    let follow_system = || system.map(Scheme::from).unwrap_or_else(|| default_scheme(branding));
    match pref {
        Theme::Light => Scheme::Light,
        Theme::Dark => Scheme::Dark,
        Theme::Auto => follow_system(),
        Theme::Account => match account_theme {
            Some(Some(true)) => Scheme::Light,
            Some(Some(false)) => Scheme::Dark,
            // Account set to "auto", or we haven't fetched `/api/me` yet.
            Some(None) | None => follow_system(),
        },
    }
}

fn default_scheme(branding: &Branding) -> Scheme {
    match branding.colors.default_scheme.as_str() {
        "light" => Scheme::Light,
        _ => Scheme::Dark,
    }
}

/// Push `branding`'s palette for `scheme` onto the egui context. Cheap enough to
/// call whenever the resolved scheme changes.
pub fn apply(ctx: &egui::Context, branding: &Branding, scheme: Scheme) {
    let palette = match scheme {
        Scheme::Light => &branding.colors.light,
        Scheme::Dark => &branding.colors.dark,
    };
    ctx.set_theme(scheme.egui());
    ctx.set_visuals_of(scheme.egui(), visuals(palette, scheme));
}

fn color(hex: &str, fallback: Color32) -> Color32 {
    Color32::from_hex(hex).unwrap_or(fallback)
}

fn visuals(p: &BrandPalette, scheme: Scheme) -> egui::Visuals {
    let mut v = match scheme {
        Scheme::Light => egui::Visuals::light(),
        Scheme::Dark => egui::Visuals::dark(),
    };

    let bg = color(&p.bg, v.panel_fill);
    let bg_elevated = color(&p.bg_elevated, v.extreme_bg_color);
    let text = color(&p.text, v.text_color());
    let text_muted = color(&p.text_muted, text);
    let border = color(&p.border, v.widgets.noninteractive.bg_stroke.color);
    let accent = color(&p.accent, v.hyperlink_color);
    let accent_hover = color(&p.accent_hover, accent);
    let danger = color(&p.danger, v.error_fg_color);
    let on_accent = color(&p.on_accent, Color32::WHITE);
    let focus_ring = color(&p.focus_ring, accent);

    v.override_text_color = Some(text);
    v.weak_text_color = Some(text_muted);
    v.hyperlink_color = accent;
    v.error_fg_color = danger;
    v.warn_fg_color = accent_hover;
    v.window_fill = bg;
    v.panel_fill = bg;
    v.faint_bg_color = bg_elevated;
    v.extreme_bg_color = bg_elevated;
    v.code_bg_color = bg_elevated;
    v.window_stroke = egui::Stroke::new(1.0, border);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = bg;
    w.noninteractive.weak_bg_fill = bg;
    w.noninteractive.bg_stroke = egui::Stroke::new(1.0, border);
    w.noninteractive.fg_stroke = egui::Stroke::new(1.0, text_muted);

    w.inactive.bg_fill = bg_elevated;
    w.inactive.weak_bg_fill = bg_elevated;
    w.inactive.bg_stroke = egui::Stroke::new(1.0, border);
    w.inactive.fg_stroke = egui::Stroke::new(1.0, text);

    w.hovered.bg_fill = bg_elevated;
    w.hovered.weak_bg_fill = bg_elevated;
    w.hovered.bg_stroke = egui::Stroke::new(1.0, accent);
    w.hovered.fg_stroke = egui::Stroke::new(1.5, text);

    w.active.bg_fill = accent;
    w.active.weak_bg_fill = accent;
    w.active.bg_stroke = egui::Stroke::new(1.0, accent_hover);
    w.active.fg_stroke = egui::Stroke::new(1.5, on_accent);

    w.open.bg_fill = bg_elevated;
    w.open.weak_bg_fill = bg_elevated;
    w.open.bg_stroke = egui::Stroke::new(1.0, border);
    w.open.fg_stroke = egui::Stroke::new(1.0, text);

    v.selection.bg_fill = accent.gamma_multiply(0.35);
    v.selection.stroke = egui::Stroke::new(1.0, focus_ring);

    v
}

/// `2032.cloud/settings` etc. built from the branding homepage.
pub fn homepage_path(branding: &Branding, path: &str) -> String {
    format!("{}/{}", branding.identity.homepage_url.trim_end_matches('/'), path.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baked_branding_parses() {
        let b = baked();
        assert_eq!(b.schema_version, 1);
        assert_eq!(b.identity.short_name, "2032");
        assert_eq!(b.colors.default_scheme, "dark");
        assert!(Color32::from_hex(&b.colors.dark.bg).is_ok());
        assert!(Color32::from_hex(&b.colors.light.accent).is_ok());
    }

    #[test]
    fn resolve_honours_explicit_preference() {
        let b = baked();
        assert_eq!(resolve(Theme::Light, None, Some(egui::Theme::Dark), &b), Scheme::Light);
        assert_eq!(resolve(Theme::Dark, Some(Some(true)), None, &b), Scheme::Dark);
    }

    #[test]
    fn resolve_account_theme_maps_true_to_light() {
        let b = baked();
        assert_eq!(resolve(Theme::Account, Some(Some(true)), None, &b), Scheme::Light);
        assert_eq!(resolve(Theme::Account, Some(Some(false)), Some(egui::Theme::Light), &b), Scheme::Dark);
    }

    #[test]
    fn resolve_falls_back_to_branding_default_when_system_unknown() {
        let b = baked();
        // default_scheme is "dark" in the baked payload.
        assert_eq!(resolve(Theme::Auto, None, None, &b), Scheme::Dark);
        assert_eq!(resolve(Theme::Account, Some(None), None, &b), Scheme::Dark);
    }

    #[test]
    fn resolve_auto_follows_system_when_known() {
        let b = baked();
        assert_eq!(resolve(Theme::Auto, None, Some(egui::Theme::Light), &b), Scheme::Light);
    }

    #[test]
    fn homepage_path_joins_cleanly() {
        let b = baked();
        assert_eq!(homepage_path(&b, "settings"), "https://2032.cloud/settings");
        assert_eq!(homepage_path(&b, "/settings"), "https://2032.cloud/settings");
    }
}
