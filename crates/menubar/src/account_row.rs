//! Account rows drawn natively. `menu::render` gives every `Entry::Account` a plain item with a plain title and
//! no action; `attach` finds those items by position and gives each one an attributed title:
//!
//! ```text
//! name [CLI] [APP]
//! 5h ▬▬── 10%   wk ▬─── 16%
//! ```
//!
//! The icons appear only for the surfaces that use the account. The item stays an ordinary enabled item, so
//! AppKit still highlights it. Tab stops line the usage columns up between rows; the percents use monospaced
//! digits.
//!
//! The icons and bars are images whose drawing handler runs each time AppKit draws them, so the colors they
//! use (`labelColor`, `quaternaryLabelColor`, the system tier colors) resolve against the menu's light or dark
//! appearance at that moment. The icons are also template images, so AppKit may tint them like text.
//!
//! When the title cannot be built (not on the main thread, the menu does not match the model), the plain
//! title stays.

use std::ffi::c_void;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSAttributedStringAttachmentConveniences, NSBezierPath, NSColor, NSCompositingOperation,
    NSFont, NSFontAttributeName, NSFontWeightRegular, NSForegroundColorAttributeName, NSImage, NSMenu, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSRectFillUsingOperation, NSTextAlignment, NSTextAttachment, NSTextTab,
};
use objc2_foundation::{NSArray, NSAttributedString, NSDictionary, NSMutableAttributedString, NSPoint, NSRange, NSRect, NSSize, NSString};

use crate::menu::{AccountRow, Bar, Entry, Tier, Usage};

/// One icon after the name.
const SLOT_WIDTH: f64 = 18.0;
const SLOT_HEIGHT: f64 = 14.0;
/// The window name (`5h`, `wk`).
const LABEL_WIDTH: f64 = 20.0;
const BAR_WIDTH: f64 = 60.0;
const BAR_HEIGHT: f64 = 6.0;
/// Between the bar and its right-aligned percent, which fits `100%`.
const BAR_GAP: f64 = 6.0;
const PERCENT_WIDTH: f64 = 36.0;
/// Between one window's percent and the next window's name.
const WINDOW_GAP: f64 = 18.0;
const WINDOW_WIDTH: f64 = LABEL_WIDTH + BAR_WIDTH + BAR_GAP + PERCENT_WIDTH + WINDOW_GAP;
/// The windows in column order; a row without one leaves its column empty.
const WINDOWS: [&str; 2] = ["5h", "wk"];
const CLI_SYMBOL: &str = "terminal";
const APP_SYMBOL: &str = "desktopcomputer";

/// Give the items of `entries`' account rows their drawn titles. `ns_menu` is the `NSMenu` that `entries`
/// were rendered into, one native item per entry.
pub fn attach(ns_menu: *mut c_void, entries: &[Entry]) {
    if MainThreadMarker::new().is_none() {
        return;
    }
    // SAFETY: `ns_menu` comes from `muda::Menu::ns_menu` on a menu the caller still owns, so it points to a
    // live `NSMenu` for the length of this call. The marker above proves we are on the main thread.
    let Some(menu) = (unsafe { (ns_menu as *const NSMenu).as_ref() }) else { return };
    walk(menu, entries, &Fonts::new());
}

struct Fonts {
    menu: Retained<NSFont>,
    small: Retained<NSFont>,
    digits: Retained<NSFont>,
}

impl Fonts {
    fn new() -> Fonts {
        let menu = NSFont::menuFontOfSize(0.0);
        // SAFETY: `NSFontWeightRegular` is an immutable AppKit constant, set before any code of ours runs.
        let regular = unsafe { NSFontWeightRegular };
        let digits = NSFont::monospacedDigitSystemFontOfSize_weight(menu.pointSize(), regular);
        Fonts { small: NSFont::systemFontOfSize(NSFont::smallSystemFontSize()), digits, menu }
    }
}

fn walk(menu: &NSMenu, entries: &[Entry], fonts: &Fonts) {
    if menu.numberOfItems() != entries.len() as isize {
        crate::log_line("the menu does not match its model; account rows stay plain text");
        return;
    }
    let style = tab_style();
    for (index, entry) in entries.iter().enumerate() {
        let Some(item) = menu.itemAtIndex(index as isize) else { continue };
        match entry {
            Entry::Account(row) => {
                item.setAttributedTitle(Some(&title(row, fonts, &style)));
                if let Usage::Bars(bars) = &row.usage {
                    let tip: Vec<String> = bars.iter().map(|b| format!("{}: {}% used, {}% left", b.window, b.used, b.remaining())).collect();
                    item.setToolTip(Some(&NSString::from_str(&tip.join(" · "))));
                }
            }
            Entry::Submenu { entries, .. } => {
                if let Some(sub) = item.submenu() {
                    walk(&sub, entries, fonts);
                }
            }
            _ => {}
        }
    }
}

/// Tab stops of the usage line: per window its name, its bar and its right-aligned percent. The first
/// window's name starts the line, so it needs no stop.
fn tab_style() -> Retained<NSMutableParagraphStyle> {
    let tab = |alignment: NSTextAlignment, at: f64| {
        // SAFETY: an empty options dictionary is valid for every alignment.
        unsafe { NSTextTab::initWithTextAlignment_location_options(NSTextTab::alloc(), alignment, at, &NSDictionary::new()) }
    };
    let mut tabs = Vec::new();
    for i in 0..WINDOWS.len() {
        let x = i as f64 * WINDOW_WIDTH;
        if i > 0 {
            tabs.push(tab(NSTextAlignment::Left, x));
        }
        tabs.push(tab(NSTextAlignment::Left, x + LABEL_WIDTH));
        tabs.push(tab(NSTextAlignment::Right, x + LABEL_WIDTH + BAR_WIDTH + BAR_GAP + PERCENT_WIDTH));
    }
    let style = NSMutableParagraphStyle::new();
    style.setTabStops(Some(&NSArray::from_retained_slice(&tabs)));
    style
}

fn title(row: &AccountRow, fonts: &Fonts, style: &NSMutableParagraphStyle) -> Retained<NSAttributedString> {
    let out = NSMutableAttributedString::new();
    // Line 1: the name, then an icon for each surface that uses the account.
    out.appendAttributedString(&text(&row.name, &fonts.menu, None));
    let icon_y = (fonts.menu.capHeight() - SLOT_HEIGHT) / 2.0;
    for symbol in [(row.cli, CLI_SYMBOL), (row.app, APP_SYMBOL)].into_iter().filter_map(|(on, s)| on.then_some(s)) {
        out.appendAttributedString(&text(" ", &fonts.menu, None));
        out.appendAttributedString(&attachment(&icon_image(Some(symbol)), rect(0.0, icon_y, SLOT_WIDTH, SLOT_HEIGHT)));
    }
    // Line 2: the usage.
    out.appendAttributedString(&text("\n", &fonts.menu, None));
    let secondary = NSColor::secondaryLabelColor();
    match &row.usage {
        Usage::Bars(bars) => {
            let bar_y = (fonts.menu.capHeight() - BAR_HEIGHT) / 2.0;
            for (i, window) in WINDOWS.into_iter().enumerate() {
                let lead = if i > 0 { "\t" } else { "" };
                match bars.iter().find(|b| b.window == window) {
                    Some(bar) => {
                        out.appendAttributedString(&text(&format!("{lead}{window}"), &fonts.small, Some(&secondary)));
                        out.appendAttributedString(&text("\t", &fonts.menu, None));
                        out.appendAttributedString(&attachment(&bar_image(bar), rect(0.0, bar_y, BAR_WIDTH, BAR_HEIGHT)));
                        out.appendAttributedString(&text(&format!("\t{}%", bar.used), &fonts.digits, None));
                    }
                    // Skip the stops of the missing window, so the next one stays in its column.
                    None => out.appendAttributedString(&text(&format!("{lead}\t\t"), &fonts.menu, None)),
                }
            }
        }
        Usage::Note(note) => out.appendAttributedString(&text(note, &fonts.small, Some(&secondary))),
    }
    let style: &AnyObject = style;
    // SAFETY: the paragraph-style key takes an `NSParagraphStyle`, and the range is the whole string.
    unsafe { out.addAttribute_value_range(NSParagraphStyleAttributeName, style, NSRange::new(0, out.length())) };
    Retained::into_super(out)
}

/// `s` in `font`; without a color it takes the menu's own text color, which also turns white when highlighted.
fn text(s: &str, font: &NSFont, color: Option<&NSColor>) -> Retained<NSAttributedString> {
    // SAFETY: AppKit's attribute-name constants are immutable and set before any code of ours runs.
    let (font_key, color_key) = unsafe { (NSFontAttributeName, NSForegroundColorAttributeName) };
    let font: &AnyObject = font;
    let attributes = match color {
        Some(color) => NSDictionary::from_slices(&[font_key, color_key], &[font, color as &AnyObject]),
        None => NSDictionary::from_slices(&[font_key], &[font]),
    };
    // SAFETY: each value has the type its key expects: an `NSFont` for the font, an `NSColor` for the color.
    unsafe { NSAttributedString::new_with_attributes(&NSString::from_str(s), &attributes) }
}

fn attachment(image: &NSImage, bounds: NSRect) -> Retained<NSAttributedString> {
    let attachment = NSTextAttachment::new();
    attachment.setImage(Some(image));
    attachment.setBounds(bounds);
    NSAttributedString::attributedStringWithAttachment(&attachment)
}

/// An icon slot: the SF Symbol `symbol` fitted and centered in it, in the label color; empty for `None`.
fn icon_image(symbol: Option<&str>) -> Retained<NSImage> {
    let symbol = symbol.and_then(|name| NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None));
    let draw = RcBlock::new(move |slot: NSRect| -> Bool {
        if let Some(symbol) = &symbol {
            let size = symbol.size();
            if size.width > 0.0 && size.height > 0.0 {
                let scale = (slot.size.width / size.width).min(slot.size.height / size.height);
                let (w, h) = (size.width * scale, size.height * scale);
                symbol.drawInRect(rect(slot.origin.x + (slot.size.width - w) / 2.0, slot.origin.y + (slot.size.height - h) / 2.0, w, h));
                // Recolor what was drawn, so the icon follows the menu's appearance even if it is not tinted.
                NSColor::labelColor().set();
                NSRectFillUsingOperation(slot, NSCompositingOperation::SourceAtop);
            }
        }
        Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(SLOT_WIDTH, SLOT_HEIGHT), false, &draw);
    image.setTemplate(true);
    image
}

/// A pill-shaped track with a pill-shaped fill as wide as the used percent, in the tier's color.
fn bar_image(bar: &Bar) -> Retained<NSImage> {
    let (fill, color) = (fill_width(bar.used), tier_color(bar.tier));
    let draw = RcBlock::new(move |track: NSRect| -> Bool {
        let radius = track.size.height / 2.0;
        NSColor::quaternaryLabelColor().setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(track, radius, radius).fill();
        if let Some(width) = fill {
            color.setFill();
            let filled = rect(track.origin.x, track.origin.y, width, track.size.height);
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(filled, radius, radius).fill();
        }
        Bool::YES
    });
    NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(BAR_WIDTH, BAR_HEIGHT), false, &draw)
}

/// How wide the fill of a bar with `used` percent used is drawn: none when nothing is used, and never
/// narrower than it is tall, so a barely used window still shows a round dot.
fn fill_width(used: u8) -> Option<f64> {
    (used > 0).then(|| (BAR_WIDTH * f64::from(used) / 100.0).max(BAR_HEIGHT))
}

fn tier_color(tier: Tier) -> Retained<NSColor> {
    match tier {
        Tier::Plenty => NSColor::systemGreenColor(),
        Tier::Low => NSColor::systemYellowColor(),
        Tier::Critical => NSColor::systemRedColor(),
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fill_follows_the_used_percent_but_stays_a_round_dot_and_vanishes_at_zero() {
        assert_eq!(fill_width(100), Some(BAR_WIDTH));
        assert_eq!(fill_width(50), Some(BAR_WIDTH / 2.0));
        assert_eq!(fill_width(1), Some(BAR_HEIGHT), "a sliver keeps its round ends");
        assert_eq!(fill_width(20), Some(BAR_WIDTH / 5.0), "20% used is a small fill, as the number says");
        assert_eq!(fill_width(0), None);
    }
}
