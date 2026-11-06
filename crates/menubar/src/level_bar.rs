//! Native level bars under the usage rows. `menu::render` gives every `Entry::Bars` a disabled text item;
//! `attach` finds those items by position and gives each one a view with a label and an `NSLevelIndicator`
//! per usage window. When it cannot (not on the main thread, an item is missing), the text item stays.
//!
//! The bar shows what is left, and its color comes from `menu::Tier`. All three fill colors are set to the
//! tier's color, so AppKit's own warning and critical ranges (which treat a high value as bad) never apply.
//! The system colors adapt to light and dark menus.

use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSColor, NSFont, NSLevelIndicator, NSLevelIndicatorStyle, NSMenu, NSTextField, NSView};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::menu::{Bar, Entry, Tier};

/// Where the first label starts, in line with the text of the rows around it.
const INSET: f64 = 21.0;
const ROW_HEIGHT: f64 = 20.0;
const LABEL_WIDTH: f64 = 54.0;
const LABEL_HEIGHT: f64 = 14.0;
const BAR_WIDTH: f64 = 72.0;
const BAR_HEIGHT: f64 = 12.0;
const GAP: f64 = 14.0;
const TRAILING: f64 = 14.0;

/// Put bar views on the items of `entries`' bar rows. `ns_menu` is the `NSMenu` that `entries` were
/// rendered into, one native item per entry.
pub fn attach(ns_menu: *mut c_void, entries: &[Entry]) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    // SAFETY: `ns_menu` comes from `muda::Menu::ns_menu` on a menu the caller still owns, so it points to a
    // live `NSMenu` for the length of this call. The marker above proves we are on the main thread.
    let Some(menu) = (unsafe { (ns_menu as *const NSMenu).as_ref() }) else { return };
    walk(menu, entries, mtm);
}

fn walk(menu: &NSMenu, entries: &[Entry], mtm: MainThreadMarker) {
    if menu.numberOfItems() != entries.len() as isize {
        crate::log_line("the menu does not match its model; usage bars stay text");
        return;
    }
    for (index, entry) in entries.iter().enumerate() {
        let Some(item) = menu.itemAtIndex(index as isize) else { continue };
        match entry {
            Entry::Bars(bars) => item.setView(Some(&row_view(bars, mtm))),
            Entry::Submenu { entries, .. } => {
                if let Some(sub) = item.submenu() {
                    walk(&sub, entries, mtm);
                }
            }
            _ => {}
        }
    }
}

fn row_view(bars: &[Bar], mtm: MainThreadMarker) -> Retained<NSView> {
    let width = INSET + bars.len() as f64 * (LABEL_WIDTH + BAR_WIDTH + GAP) - GAP + TRAILING;
    let row = NSView::initWithFrame(mtm.alloc(), rect(0.0, 0.0, width, ROW_HEIGHT));
    let tip: Vec<String> = bars.iter().map(|b| format!("{}: {}% used, {}% left", b.window, b.used, b.remaining())).collect();
    row.setToolTip(Some(&NSString::from_str(&tip.join(" · "))));
    let mut x = INSET;
    for bar in bars {
        let label = NSTextField::labelWithString(&NSString::from_str(&format!("{} {}%", bar.window, bar.used)), mtm);
        label.setFont(Some(&NSFont::systemFontOfSize(NSFont::smallSystemFontSize())));
        label.setTextColor(Some(&NSColor::secondaryLabelColor()));
        label.setFrame(rect(x, (ROW_HEIGHT - LABEL_HEIGHT) / 2.0, LABEL_WIDTH, LABEL_HEIGHT));
        row.addSubview(&label);
        x += LABEL_WIDTH;

        let level = NSLevelIndicator::initWithFrame(mtm.alloc(), rect(x, (ROW_HEIGHT - BAR_HEIGHT) / 2.0, BAR_WIDTH, BAR_HEIGHT));
        level.setLevelIndicatorStyle(NSLevelIndicatorStyle::ContinuousCapacity);
        level.setEditable(false);
        level.setMinValue(0.0);
        level.setMaxValue(100.0);
        level.setDoubleValue(f64::from(bar.remaining()));
        let color = tier_color(bar.tier);
        level.setFillColor(Some(&color));
        level.setWarningFillColor(Some(&color));
        level.setCriticalFillColor(Some(&color));
        row.addSubview(&level);
        x += BAR_WIDTH + GAP;
    }
    row
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
