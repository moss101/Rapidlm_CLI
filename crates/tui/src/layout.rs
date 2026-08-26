//! Responsive pane geometry for the interactive TUI.
//!
//! Pure projection: [`compute_layout`] maps a terminal [`Rect`] and [`UiMode`]
//! to region bounds. It does not own domain state or issue kernel commands.
//!
//! Under 100 columns, requested sidebars collapse (PRD: panels become tabs).
//! Terminals smaller than 80x24 still receive a non-overlapping minimum-size
//! fallback rather than inventing extra cells.

use crate::state::{AppState, UiRoute};

/// Status line is always one row when any height is available.
pub const STATUS_HEIGHT: u16 = 1;

/// Default composer rows when the terminal has room.
pub const DEFAULT_COMPOSER_HEIGHT: u16 = 3;

/// Composer never grows past this many rows from the layout engine.
pub const MAX_COMPOSER_HEIGHT: u16 = 8;

/// PRD: under 100 columns, side panels collapse into tabs.
pub const SIDEBAR_COLLAPSE_WIDTH: u16 = 100;

/// Narrowest main column that may sit beside a sidebar.
pub const MIN_MAIN_WIDTH: u16 = 40;

/// Sidebar width on 100–119 columns.
pub const SIDEBAR_NARROW_WIDTH: u16 = 24;

/// Sidebar width on 120–199 columns.
pub const SIDEBAR_STANDARD_WIDTH: u16 = 36;

/// Sidebar width on 200+ columns.
pub const SIDEBAR_WIDE_WIDTH: u16 = 48;

/// Preferred modal inset when the content area is large enough.
pub const MODAL_INSET: u16 = 2;

/// Inclusive-origin exclusive-end rectangle in terminal cells.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct Rect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

/// Visible chrome configuration for [`compute_layout`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum UiMode {
    /// Transcript + composer + status. No sidebar.
    #[default]
    Transcript,
    /// Sidebar requested beside the transcript.
    Sidebar,
    /// Modal overlay on the transcript chrome.
    Modal,
    /// Sidebar requested and a modal overlay is open.
    SidebarModal,
}

/// Allocated bounds for each layout region. Empty rects are hidden.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct LayoutRects {
    transcript: Rect,
    composer: Rect,
    status: Rect,
    sidebar: Rect,
    modal: Rect,
}

impl Rect {
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub const fn empty() -> Self {
        Self::new(0, 0, 0, 0)
    }

    pub const fn x(self) -> u16 {
        self.x
    }

    pub const fn y(self) -> u16 {
        self.y
    }

    pub const fn width(self) -> u16 {
        self.width
    }

    pub const fn height(self) -> u16 {
        self.height
    }

    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub const fn right(self) -> u16 {
        self.x.saturating_add(self.width)
    }

    pub const fn bottom(self) -> u16 {
        self.y.saturating_add(self.height)
    }

    pub const fn area(self) -> u32 {
        (self.width as u32).saturating_mul(self.height as u32)
    }

    /// True when `other` is empty or fully inside this rect.
    pub const fn contains_rect(self, other: Self) -> bool {
        other.is_empty()
            || (other.x >= self.x
                && other.y >= self.y
                && other.right() <= self.right()
                && other.bottom() <= self.bottom())
    }

    pub fn intersection(self, other: Self) -> Self {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= x || bottom <= y {
            Self::empty()
        } else {
            Self::new(x, y, right.saturating_sub(x), bottom.saturating_sub(y))
        }
    }

    pub fn intersects(self, other: Self) -> bool {
        !self.intersection(other).is_empty()
    }
}

impl UiMode {
    pub const fn shows_sidebar(self) -> bool {
        matches!(self, Self::Sidebar | Self::SidebarModal)
    }

    pub const fn shows_modal(self) -> bool {
        matches!(self, Self::Modal | Self::SidebarModal)
    }

    /// Chrome from the frontend projection. Domain maps stay unused.
    pub fn from_state(state: &AppState) -> Self {
        let sidebar = !matches!(state.route(), UiRoute::Transcript);
        let modal = !state.modal_stack().is_empty();
        match (sidebar, modal) {
            (false, false) => Self::Transcript,
            (true, false) => Self::Sidebar,
            (false, true) => Self::Modal,
            (true, true) => Self::SidebarModal,
        }
    }
}

impl LayoutRects {
    pub const fn transcript(self) -> Rect {
        self.transcript
    }

    pub const fn composer(self) -> Rect {
        self.composer
    }

    pub const fn status(self) -> Rect {
        self.status
    }

    pub const fn sidebar(self) -> Rect {
        self.sidebar
    }

    pub const fn modal(self) -> Rect {
        self.modal
    }

    /// Stable one-line dump used by golden tests.
    pub fn golden(self) -> String {
        format!(
            "transcript={} composer={} status={} sidebar={} modal={}",
            fmt_rect(self.transcript),
            fmt_rect(self.composer),
            fmt_rect(self.status),
            fmt_rect(self.sidebar),
            fmt_rect(self.modal),
        )
    }
}

/// Allocate transcript / composer / status / sidebar / modal inside `area`.
///
/// Never returns negative or overflowing composer bounds. Terminals smaller
/// than the preferred 80x24 still get a squeezed, non-overlapping fallback.
pub fn compute_layout(area: Rect, mode: UiMode) -> LayoutRects {
    compute_layout_with_composer(area, mode, DEFAULT_COMPOSER_HEIGHT)
}

/// Same as [`compute_layout`] with an explicit composer row request.
pub fn compute_layout_with_composer(area: Rect, mode: UiMode, composer_lines: u16) -> LayoutRects {
    if area.is_empty() {
        return LayoutRects::default();
    }

    let (body, status) = split_bottom(area, STATUS_HEIGHT.min(area.height));
    let composer_height = composer_height(body.height, composer_lines);
    let (main, composer) = split_bottom(body, composer_height);

    let (transcript, sidebar) = if mode.shows_sidebar() {
        split_sidebar(main)
    } else {
        (main, Rect::empty())
    };

    let modal = if mode.shows_modal() {
        modal_bounds(body)
    } else {
        Rect::empty()
    };

    LayoutRects {
        transcript,
        composer,
        status,
        sidebar,
        modal,
    }
}

fn composer_height(body_height: u16, requested: u16) -> u16 {
    if body_height == 0 {
        return 0;
    }
    let want = requested.clamp(1, MAX_COMPOSER_HEIGHT);
    if body_height == 1 {
        1
    } else {
        // Keep at least one row for the transcript/sidebar column.
        want.min(body_height.saturating_sub(1))
    }
}

fn sidebar_width(main_width: u16) -> u16 {
    if main_width < SIDEBAR_COLLAPSE_WIDTH {
        return 0;
    }
    let candidate = if main_width < 120 {
        SIDEBAR_NARROW_WIDTH
    } else if main_width < 200 {
        SIDEBAR_STANDARD_WIDTH
    } else {
        SIDEBAR_WIDE_WIDTH
    };
    if main_width.saturating_sub(candidate) < MIN_MAIN_WIDTH {
        0
    } else {
        candidate
    }
}

fn split_bottom(area: Rect, bottom_height: u16) -> (Rect, Rect) {
    if area.is_empty() {
        return (Rect::empty(), Rect::empty());
    }
    let bottom_height = bottom_height.min(area.height);
    let top_height = area.height.saturating_sub(bottom_height);
    let top = Rect::new(area.x, area.y, area.width, top_height);
    let bottom = Rect::new(
        area.x,
        area.y.saturating_add(top_height),
        area.width,
        bottom_height,
    );
    (top, bottom)
}

fn split_sidebar(area: Rect) -> (Rect, Rect) {
    let width = sidebar_width(area.width);
    if width == 0 || area.is_empty() {
        return (area, Rect::empty());
    }
    let main_width = area.width.saturating_sub(width);
    let main = Rect::new(area.x, area.y, main_width, area.height);
    let sidebar = Rect::new(
        area.x.saturating_add(main_width),
        area.y,
        width,
        area.height,
    );
    (main, sidebar)
}

fn modal_bounds(content: Rect) -> Rect {
    if content.is_empty() {
        return Rect::empty();
    }
    let inset = if content.width > 4 && content.height > 4 {
        MODAL_INSET
    } else if content.width > 2 && content.height > 2 {
        1
    } else {
        0
    };
    let inset_x2 = inset.saturating_mul(2);
    Rect::new(
        content.x.saturating_add(inset),
        content.y.saturating_add(inset),
        content.width.saturating_sub(inset_x2),
        content.height.saturating_sub(inset_x2),
    )
}

fn fmt_rect(rect: Rect) -> String {
    if rect.is_empty() {
        "-".to_owned()
    } else {
        format!("{}+{},{}x{}", rect.x, rect.y, rect.width, rect.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{LocalUiEvent, UiEvent, reduce};

    const GOLDEN_80X24_TRANSCRIPT: &str =
        "transcript=0+0,80x20 composer=0+20,80x3 status=0+23,80x1 sidebar=- modal=-";
    const GOLDEN_80X24_SIDEBAR: &str =
        "transcript=0+0,80x20 composer=0+20,80x3 status=0+23,80x1 sidebar=- modal=-";
    const GOLDEN_80X24_MODAL: &str =
        "transcript=0+0,80x20 composer=0+20,80x3 status=0+23,80x1 sidebar=- modal=2+2,76x19";
    const GOLDEN_120X40_TRANSCRIPT: &str =
        "transcript=0+0,120x36 composer=0+36,120x3 status=0+39,120x1 sidebar=- modal=-";
    const GOLDEN_120X40_SIDEBAR: &str =
        "transcript=0+0,84x36 composer=0+36,120x3 status=0+39,120x1 sidebar=84+0,36x36 modal=-";
    const GOLDEN_120X40_SIDEBAR_MODAL: &str = "transcript=0+0,84x36 composer=0+36,120x3 status=0+39,120x1 sidebar=84+0,36x36 modal=2+2,116x35";
    const GOLDEN_200X60_TRANSCRIPT: &str =
        "transcript=0+0,200x56 composer=0+56,200x3 status=0+59,200x1 sidebar=- modal=-";
    const GOLDEN_200X60_SIDEBAR: &str =
        "transcript=0+0,152x56 composer=0+56,200x3 status=0+59,200x1 sidebar=152+0,48x56 modal=-";
    const GOLDEN_200X60_MODAL: &str =
        "transcript=0+0,200x56 composer=0+56,200x3 status=0+59,200x1 sidebar=- modal=2+2,196x55";

    fn area(width: u16, height: u16) -> Rect {
        Rect::new(0, 0, width, height)
    }

    fn assert_contained(area: Rect, layout: LayoutRects) {
        assert!(
            area.contains_rect(layout.transcript()),
            "transcript outside"
        );
        assert!(area.contains_rect(layout.composer()), "composer outside");
        assert!(area.contains_rect(layout.status()), "status outside");
        assert!(area.contains_rect(layout.sidebar()), "sidebar outside");
        assert!(area.contains_rect(layout.modal()), "modal outside");
    }

    fn assert_nonoverlapping_chrome(layout: LayoutRects) {
        let chrome = [
            layout.transcript(),
            layout.composer(),
            layout.status(),
            layout.sidebar(),
        ];
        for (i, a) in chrome.iter().enumerate() {
            for b in chrome.iter().skip(i + 1) {
                assert!(
                    !a.intersects(*b),
                    "overlap {a:?} vs {b:?} in {}",
                    layout.golden()
                );
            }
        }
    }

    fn assert_composer_valid(area: Rect, layout: LayoutRects) {
        let composer = layout.composer();
        if composer.is_empty() {
            return;
        }
        assert!(area.contains_rect(composer), "composer not in area");
        assert!(!composer.intersects(layout.status()) || layout.status().is_empty());
        assert!(!composer.intersects(layout.transcript()) || layout.transcript().is_empty());
        assert!(!composer.intersects(layout.sidebar()) || layout.sidebar().is_empty());
        assert_eq!(
            composer.x().saturating_add(composer.width()),
            composer.right()
        );
        assert_eq!(
            composer.y().saturating_add(composer.height()),
            composer.bottom()
        );
    }

    #[test]
    fn golden_80x24() {
        let frame = area(80, 24);
        assert_eq!(
            compute_layout(frame, UiMode::Transcript).golden(),
            GOLDEN_80X24_TRANSCRIPT
        );
        assert_eq!(
            compute_layout(frame, UiMode::Sidebar).golden(),
            GOLDEN_80X24_SIDEBAR
        );
        assert_eq!(
            compute_layout(frame, UiMode::Modal).golden(),
            GOLDEN_80X24_MODAL
        );
        assert_eq!(
            compute_layout(frame, UiMode::SidebarModal).golden(),
            GOLDEN_80X24_MODAL
        );
    }

    #[test]
    fn golden_120x40() {
        let frame = area(120, 40);
        assert_eq!(
            compute_layout(frame, UiMode::Transcript).golden(),
            GOLDEN_120X40_TRANSCRIPT
        );
        assert_eq!(
            compute_layout(frame, UiMode::Sidebar).golden(),
            GOLDEN_120X40_SIDEBAR
        );
        assert_eq!(
            compute_layout(frame, UiMode::SidebarModal).golden(),
            GOLDEN_120X40_SIDEBAR_MODAL
        );
    }

    #[test]
    fn golden_200x60() {
        let frame = area(200, 60);
        assert_eq!(
            compute_layout(frame, UiMode::Transcript).golden(),
            GOLDEN_200X60_TRANSCRIPT
        );
        assert_eq!(
            compute_layout(frame, UiMode::Sidebar).golden(),
            GOLDEN_200X60_SIDEBAR
        );
        assert_eq!(
            compute_layout(frame, UiMode::Modal).golden(),
            GOLDEN_200X60_MODAL
        );
    }

    #[test]
    fn composer_never_negative_or_overlapping() {
        let modes = [
            UiMode::Transcript,
            UiMode::Sidebar,
            UiMode::Modal,
            UiMode::SidebarModal,
        ];
        for width in 0..=220 {
            for height in 0..=70 {
                for mode in modes {
                    let frame = area(width, height);
                    let layout = compute_layout(frame, mode);
                    assert_contained(frame, layout);
                    assert_nonoverlapping_chrome(layout);
                    assert_composer_valid(frame, layout);
                }
            }
        }
    }

    #[test]
    fn offset_origin_is_preserved() {
        let frame = Rect::new(7, 4, 120, 40);
        let layout = compute_layout(frame, UiMode::Sidebar);
        assert_eq!(layout.status().x(), 7);
        assert_eq!(layout.status().y(), 43);
        assert_eq!(layout.composer().x(), 7);
        assert_eq!(layout.transcript().x(), 7);
        assert_eq!(layout.sidebar().x(), 7 + 84);
        assert_contained(frame, layout);
        assert_nonoverlapping_chrome(layout);
        assert_composer_valid(frame, layout);
    }

    #[test]
    fn sidebar_collapses_under_100_columns() {
        let layout = compute_layout(area(99, 24), UiMode::Sidebar);
        assert!(layout.sidebar().is_empty());
        assert_eq!(layout.transcript().width(), 99);
        let opened = compute_layout(area(100, 24), UiMode::Sidebar);
        assert_eq!(opened.sidebar().width(), SIDEBAR_NARROW_WIDTH);
        assert_eq!(opened.transcript().width(), 76);
    }

    #[test]
    fn min_size_fallback_squeezes_without_overlap() {
        let tiny = compute_layout(area(10, 4), UiMode::SidebarModal);
        assert_eq!(tiny.status(), Rect::new(0, 3, 10, 1));
        assert_eq!(tiny.composer(), Rect::new(0, 1, 10, 2));
        assert_eq!(tiny.transcript(), Rect::new(0, 0, 10, 1));
        assert!(tiny.sidebar().is_empty());
        assert!(!tiny.modal().is_empty());
        assert_contained(area(10, 4), tiny);
        assert_nonoverlapping_chrome(tiny);
        assert_composer_valid(area(10, 4), tiny);

        let one = compute_layout(area(8, 1), UiMode::Transcript);
        assert_eq!(one.status(), Rect::new(0, 0, 8, 1));
        assert!(one.composer().is_empty());
        assert!(one.transcript().is_empty());

        assert_eq!(
            compute_layout(Rect::empty(), UiMode::SidebarModal),
            LayoutRects::default()
        );
    }

    #[test]
    fn requested_composer_rows_are_clamped() {
        let tall = compute_layout_with_composer(area(80, 24), UiMode::Transcript, 99);
        assert_eq!(tall.composer().height(), MAX_COMPOSER_HEIGHT);
        assert_eq!(tall.transcript().height(), 15);
        let short = compute_layout_with_composer(area(80, 24), UiMode::Transcript, 0);
        assert_eq!(short.composer().height(), 1);
    }

    #[test]
    fn from_state_maps_route() {
        let mut state = AppState::new();
        assert_eq!(UiMode::from_state(&state), UiMode::Transcript);
        state = reduce(
            state,
            &UiEvent::Local(LocalUiEvent::SetRoute(UiRoute::Agents)),
        );
        assert_eq!(UiMode::from_state(&state), UiMode::Sidebar);
        state = reduce(
            state,
            &UiEvent::Local(LocalUiEvent::SetRoute(UiRoute::Transcript)),
        );
        assert_eq!(UiMode::from_state(&state), UiMode::Transcript);
    }
}
