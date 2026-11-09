//! Account rows drawn natively. `menu::render` gives every `Entry::Account` a submenu item with a plain title;
//! `attach` finds those items by position, at any depth, and gives each one an attributed title on one line:
//!
//! ```text
//! name [CLI] [APP]   5h ▬▬── 10%   wk ▬─── 16%
//! ```
//!
//! The icons appear only for the surfaces that use the account. Tab stops line the usage columns up between
//! rows: the first column is as wide as the widest name with its icons in the whole menu, and a name wider
//! than `NAME_MAX_WIDTH` is cut with `…`. The percents use monospaced digits.
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
use objc2::{msg_send, AnyThread, MainThreadMarker};
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
const BAR_WIDTH: f64 = 40.0;
const BAR_HEIGHT: f64 = 6.0;
/// Between the bar and its right-aligned percent, which fits `100%`.
const BAR_GAP: f64 = 6.0;
const PERCENT_WIDTH: f64 = 36.0;
/// Between one window's percent and the next window's name.
const WINDOW_GAP: f64 = 14.0;
const WINDOW_WIDTH: f64 = LABEL_WIDTH + BAR_WIDTH + BAR_GAP + PERCENT_WIDTH + WINDOW_GAP;
/// The widest a name may be drawn, in points.
const NAME_MAX_WIDTH: f64 = 220.0;
/// Between the name column and the usage columns.
const NAME_GAP: f64 = 16.0;
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
    let fonts = Fonts::new();
    let layout = Layout::new(entries, &fonts);
    walk(menu, entries, &fonts, &layout);
}

/// What the rows of one menu share: the cut names and the width of the name column.
struct Layout {
    style: Retained<NSMutableParagraphStyle>,
}

impl Layout {
    fn new(entries: &[Entry], fonts: &Fonts) -> Layout {
        let icon = width(" ", &fonts.menu) + SLOT_WIDTH;
        let mut column: f64 = 0.0;
        rows(entries, &mut |row| column = column.max(name_width(row, fonts, icon)));
        Layout { style: tab_style(column + NAME_GAP) }
    }
}

/// Call `f` on every account row of `entries`, at any depth.
fn rows(entries: &[Entry], f: &mut dyn FnMut(&AccountRow)) {
    for entry in entries {
        match entry {
            Entry::Account(row) => {
                f(row);
                rows(&row.children, f);
            }
            Entry::Submenu { entries, .. } => rows(entries, f),
            _ => {}
        }
    }
}

/// The drawn width of `s` in `font`.
fn width(s: &str, font: &NSFont) -> f64 {
    let attributed = text(s, font, None);
    // SAFETY: `size` is the AppKit string-drawing method of `NSAttributedString`; it takes no arguments and
    // returns an `NSSize`. It is called through `msg_send` because the crate feature that declares it is not enabled.
    let size: NSSize = unsafe { msg_send![&*attributed, size] };
    size.width
}

/// The name of `row`, cut with `…` until it is at most `NAME_MAX_WIDTH` wide.
fn fitted_name(row: &AccountRow, font: &NSFont) -> String {
    let mut name = row.name.clone();
    if width(&name, font) <= NAME_MAX_WIDTH {
        return name;
    }
    while name.pop().is_some() {
        let cut = format!("{}…", name.trim_end());
        if width(&cut, font) <= NAME_MAX_WIDTH {
            return cut;
        }
    }
    "…".to_string()
}

/// The width of the name and the icons of `row`.
fn name_width(row: &AccountRow, fonts: &Fonts, icon: f64) -> f64 {
    width(&fitted_name(row, &fonts.menu), &fonts.menu) + icon * [row.cli, row.app].into_iter().filter(|on| *on).count() as f64
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

fn walk(menu: &NSMenu, entries: &[Entry], fonts: &Fonts, layout: &Layout) {
    if menu.numberOfItems() != entries.len() as isize {
        crate::log_line("the menu does not match its model; account rows stay plain text");
        return;
    }
    for (index, entry) in entries.iter().enumerate() {
        let Some(item) = menu.itemAtIndex(index as isize) else { continue };
        match entry {
            Entry::Account(row) => {
                item.setAttributedTitle(Some(&title(row, fonts, layout)));
                if let Usage::Bars(bars) = &row.usage {
                    let tip: Vec<String> = bars.iter().map(|b| format!("{}: {}% used, {}% left", b.window, b.used, b.remaining())).collect();
                    item.setToolTip(Some(&NSString::from_str(&tip.join(" · "))));
                }
                if let Some(sub) = item.submenu() {
                    walk(&sub, &row.children, fonts, layout);
                }
            }
            Entry::Submenu { entries, .. } => {
                if let Some(sub) = item.submenu() {
                    walk(&sub, entries, fonts, layout);
                }
            }
            _ => {}
        }
    }
}

/// Tab stops of the row: the usage starts at `column`, and per window there are stops for its name, its bar
/// and its right-aligned percent.
fn tab_style(column: f64) -> Retained<NSMutableParagraphStyle> {
    let tab = |alignment: NSTextAlignment, at: f64| {
        // SAFETY: an empty options dictionary is valid for every alignment.
        unsafe { NSTextTab::initWithTextAlignment_location_options(NSTextTab::alloc(), alignment, at, &NSDictionary::new()) }
    };
    let mut tabs = Vec::new();
    for i in 0..WINDOWS.len() {
        let x = column + i as f64 * WINDOW_WIDTH;
        tabs.push(tab(NSTextAlignment::Left, x));
        tabs.push(tab(NSTextAlignment::Left, x + LABEL_WIDTH));
        tabs.push(tab(NSTextAlignment::Right, x + LABEL_WIDTH + BAR_WIDTH + BAR_GAP + PERCENT_WIDTH));
    }
    let style = NSMutableParagraphStyle::new();
    style.setTabStops(Some(&NSArray::from_retained_slice(&tabs)));
    style
}

fn title(row: &AccountRow, fonts: &Fonts, layout: &Layout) -> Retained<NSAttributedString> {
    let out = NSMutableAttributedString::new();
    // The name, then an icon for each surface that uses the account.
    out.appendAttributedString(&text(&fitted_name(row, &fonts.menu), &fonts.menu, None));
    let icon_y = (fonts.menu.capHeight() - SLOT_HEIGHT) / 2.0;
    for symbol in [(row.cli, CLI_SYMBOL), (row.app, APP_SYMBOL)].into_iter().filter_map(|(on, s)| on.then_some(s)) {
        out.appendAttributedString(&text(" ", &fonts.menu, None));
        out.appendAttributedString(&attachment(&icon_image(Some(symbol)), rect(0.0, icon_y, SLOT_WIDTH, SLOT_HEIGHT)));
    }
    // The usage, after the name column.
    let secondary = NSColor::secondaryLabelColor();
    match &row.usage {
        Usage::Bars(bars) => {
            let bar_y = (fonts.menu.capHeight() - BAR_HEIGHT) / 2.0;
            for window in WINDOWS {
                match bars.iter().find(|b| b.window == window) {
                    Some(bar) => {
                        out.appendAttributedString(&text(&format!("\t{window}"), &fonts.small, Some(&secondary)));
                        out.appendAttributedString(&text("\t", &fonts.menu, None));
                        out.appendAttributedString(&attachment(&bar_image(bar), rect(0.0, bar_y, BAR_WIDTH, BAR_HEIGHT)));
                        out.appendAttributedString(&text(&format!("\t{}%", bar.used), &fonts.digits, None));
                    }
                    // Skip the stops of the missing window, so the next one stays in its column.
                    None => out.appendAttributedString(&text("\t\t\t", &fonts.menu, None)),
                }
            }
        }
        Usage::Note(note) => out.appendAttributedString(&text(&format!("\t{note}"), &fonts.small, Some(&secondary))),
    }
    let style: &AnyObject = &layout.style;
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
