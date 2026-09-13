//! `suh browser <tool> [flags]` — thin CLI over `sudohand-browser`, one
//! subcommand per tool, names identical to adb / the Python
//! `ai_dev_browser.tools.<name>` modules (kebab-case aliases too).
//! Output is JSON on stdout; failures use the sudohand error envelope.

#![allow(clippy::struct_excessive_bools, clippy::too_many_lines)]

use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use serde_json::{json, Value};
use sudohand_browser::actions::TypeOptions;
use sudohand_browser::browser::{Reuse, StartOptions};
use sudohand_browser::chrome::Headless;
use sudohand_browser::connection::{connect_browser, get_active_tab, BrowserClient, Tab};
use sudohand_browser::dialog::DialogOptions;
use sudohand_browser::elements::{ScrollOptions, TypeByTextOptions};
use sudohand_browser::image_cap::ImageCap;
use sudohand_browser::mouse::ClickOptions;
use sudohand_browser::page::{PdfOptions, ScreenshotOptions};
use sudohand_browser::snapshot::DiscoverOptions;

/// Connection-scope flags every tab-taking tool accepts.
#[derive(Args, Debug, Clone)]
pub struct Conn {
    /// Chrome debugging port (auto-detects: AI_DEV_BROWSER_PORT → workspace scan → 9350)
    #[arg(short, long)]
    port: Option<u16>,
    /// Act on the tab whose URL contains this substring (or AI_DEV_BROWSER_TAB_URL)
    #[arg(long)]
    tab_url: Option<String>,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum ReuseArg {
    None,
    Any,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Start a browser instance — isolated (temp profile) unless --profile is given
    #[command(name = "browser_start", alias = "browser-start")]
    BrowserStart {
        /// Debug port (auto-assigned if omitted)
        #[arg(long)]
        port: Option<u16>,
        /// Headless mode: `new` (default when flag given), `old`, `1`/`true`; falls back to AI_DEV_BROWSER_HEADLESS
        #[arg(long, num_args = 0..=1, default_missing_value = "new")]
        headless: Option<String>,
        /// Initial URL (default about:blank)
        #[arg(long)]
        url: Option<String>,
        /// Named persistent profile (login survives, same-profile calls reuse)
        #[arg(long)]
        profile: Option<String>,
        /// Force a temporary profile
        #[arg(long)]
        temp: bool,
        /// Reuse strategy for a named profile
        #[arg(long, value_enum, default_value_t = ReuseArg::Any)]
        reuse: ReuseArg,
        /// Seconds to wait for the debug port
        #[arg(long, default_value_t = 30.0)]
        startup_timeout: f64,
        /// Extra Chrome flags appended after the defaults
        #[arg(long, num_args = 0..)]
        extra_args: Vec<String>,
        /// Override/remove default flags as JSON: '{"--flag": null, "--other": "v"}'
        #[arg(long)]
        override_default_args: Option<String>,
        /// Route Chrome's stderr to null
        #[arg(long)]
        silent_stderr: bool,
    },
    /// Stop browser instance(s)
    #[command(name = "browser_stop", alias = "browser-stop")]
    BrowserStop {
        /// Port of the browser to stop
        #[arg(long)]
        port: Option<u16>,
        /// Stop every debugging Chrome
        #[arg(long)]
        stop_all: bool,
    },
    /// List debugging Chrome instances
    #[command(name = "browser_list", alias = "browser-list")]
    BrowserList {
        /// Show Chromes from all workspaces
        #[arg(long)]
        all_workspaces: bool,
    },
    /// Navigate to a URL
    #[command(name = "page_goto", alias = "page-goto")]
    PageGoto {
        #[command(flatten)]
        conn: Conn,
        /// URL to load
        #[arg(long)]
        url: String,
        /// Open in a new tab
        #[arg(long)]
        tab_new: bool,
        /// Don't wait for document.readyState === "complete"
        #[arg(long = "no-wait", action = clap::ArgAction::SetFalse)]
        wait: bool,
    },
    /// Discover interactable elements (accessibility tree + DOM scan) with refs
    #[command(name = "page_discover", alias = "page-discover")]
    PageDiscover {
        #[command(flatten)]
        conn: Conn,
        /// Case-insensitive substring filter on the element name
        #[arg(long)]
        text: Option<String>,
        /// Include non-interactive nodes too
        #[arg(long = "no-interactable-only", action = clap::ArgAction::SetFalse)]
        interactable_only: bool,
        /// Skip x/y/box
        #[arg(long = "no-include-coordinates", action = clap::ArgAction::SetFalse)]
        include_coordinates: bool,
        /// Skip same-origin iframes
        #[arg(long = "no-include-iframes", action = clap::ArgAction::SetFalse)]
        include_iframes: bool,
        /// Skip the DOM scan (pure accessibility-tree results)
        #[arg(long = "no-dom-scan", action = clap::ArgAction::SetFalse)]
        dom_scan: bool,
        /// Max DOM-scanned elements
        #[arg(long, default_value_t = 200)]
        dom_limit: usize,
    },
    /// Capture a screenshot scaled for LLM vision, with coordinate metadata embedded
    #[command(name = "page_screenshot", alias = "page-screenshot")]
    PageScreenshot {
        #[command(flatten)]
        conn: Conn,
        /// Output path (default $AI_DEV_BROWSER_OUTPUT_DIR or ./output/{timestamp}.png)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Capture beyond the viewport
        #[arg(long)]
        full_page: bool,
        /// Don't rescale to CSS pixels / caps
        #[arg(long = "no-css-scale", action = clap::ArgAction::SetFalse)]
        css_scale: bool,
        /// Long-edge cap in px (0 disables)
        #[arg(long, default_value_t = sudohand_browser::geometry::MAX_SCREENSHOT_LONG_EDGE)]
        max_long_edge: u32,
        /// Total-pixel cap (0 disables)
        #[arg(long, default_value_t = sudohand_browser::geometry::MAX_SCREENSHOT_TOTAL_PIXELS)]
        max_total_pixels: u64,
        /// Per-call cap as JSON: '{"max_bytes": 200000, "max_dimension": 1024}' (JPEG when max_bytes)
        #[arg(long)]
        image_cap: Option<String>,
    },
    /// Current URL / title / readyState without a discover
    #[command(name = "page_info", alias = "page-info")]
    PageInfo {
        #[command(flatten)]
        conn: Conn,
    },
    /// Reload the page
    #[command(name = "page_reload", alias = "page-reload")]
    PageReload {
        #[command(flatten)]
        conn: Conn,
        /// Use the cache
        #[arg(long = "no-ignore-cache", action = clap::ArgAction::SetFalse)]
        ignore_cache: bool,
    },
    /// Wait until the URL matches (substring / regex, or exact)
    #[command(name = "page_wait_url", alias = "page-wait-url")]
    PageWaitUrl {
        #[command(flatten)]
        conn: Conn,
        /// URL substring or regex
        #[arg(long)]
        pattern: Option<String>,
        /// Exact URL
        #[arg(long)]
        exact: Option<String>,
        /// Max seconds to wait
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
    },
    /// Wait for an element to be visible (CSS selector or visible text) and return its ref
    #[command(name = "page_wait_element", alias = "page-wait-element")]
    PageWaitElement {
        #[command(flatten)]
        conn: Conn,
        /// Visible text (top frame)
        #[arg(long)]
        text: Option<String>,
        /// CSS selector (ARIA-less / datarole controls)
        #[arg(long)]
        selector: Option<String>,
        /// Max seconds to wait
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
    },
    /// Scroll: incremental gesture, to top/bottom of the real scroller, or to an element by text
    #[command(name = "page_scroll", alias = "page-scroll")]
    PageScroll {
        #[command(flatten)]
        conn: Conn,
        /// up / down
        #[arg(long, default_value = "down")]
        direction: String,
        /// Percent of the viewport height
        #[arg(long, default_value_t = 25)]
        amount: i64,
        /// Scroll the scrollable container to its bottom
        #[arg(long)]
        to_bottom: bool,
        /// Scroll the scrollable container to its top
        #[arg(long)]
        to_top: bool,
        /// Visible text of an element to scroll into view
        #[arg(long)]
        to_element: Option<String>,
    },
    /// Print the page to PDF (headless only)
    #[command(name = "page_pdf", alias = "page-pdf")]
    PagePdf {
        #[command(flatten)]
        conn: Conn,
        /// Output path (default $AI_DEV_BROWSER_OUTPUT_DIR or ./output/{timestamp}.pdf)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Landscape orientation
        #[arg(long)]
        landscape: bool,
        /// Skip background graphics
        #[arg(long = "no-print-background", action = clap::ArgAction::SetFalse)]
        print_background: bool,
        /// Rendering scale
        #[arg(long, default_value_t = 1.0)]
        scale: f64,
        /// Paper width in inches
        #[arg(long, default_value_t = 8.5)]
        paper_width: f64,
        /// Paper height in inches
        #[arg(long, default_value_t = 11.0)]
        paper_height: f64,
        /// Top margin in inches
        #[arg(long, default_value_t = 0.0)]
        margin_top: f64,
        /// Bottom margin in inches
        #[arg(long, default_value_t = 0.0)]
        margin_bottom: f64,
        /// Left margin in inches
        #[arg(long, default_value_t = 0.0)]
        margin_left: f64,
        /// Right margin in inches
        #[arg(long, default_value_t = 0.0)]
        margin_right: f64,
        /// Page ranges, e.g. "1-5" or "1,3,5-9"
        #[arg(long, default_value = "")]
        page_ranges: String,
    },
    /// Make the page behave as if its window were focused
    #[command(name = "page_emulate_focus", alias = "page-emulate-focus")]
    PageEmulateFocus {
        #[command(flatten)]
        conn: Conn,
        /// Disable focus emulation
        #[arg(long = "no-enabled", action = clap::ArgAction::SetFalse)]
        enabled: bool,
    },
    /// Wait until document.readyState === "complete"
    #[command(name = "page_wait_ready", alias = "page-wait-ready")]
    PageWaitReady {
        #[command(flatten)]
        conn: Conn,
        /// Max seconds to wait
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
        /// Extra settle time after load
        #[arg(long, default_value_t = 0.5)]
        idle_time: f64,
    },
    /// Whole-document HTML
    #[command(name = "page_html", alias = "page-html")]
    PageHtml {
        #[command(flatten)]
        conn: Conn,
        /// outerHTML of the document element instead of innerHTML
        #[arg(long)]
        outer: bool,
    },
    /// Click an element by ref from page_discover
    #[command(name = "click_by_ref", alias = "click-by-ref")]
    ClickByRef {
        #[command(flatten)]
        conn: Conn,
        /// Element ref, e.g. "5#214" or "FRAME_ABC123:5#214"
        #[arg(long = "ref")]
        r#ref: String,
        /// Bare press/release at the box centre instead of the human-like actuator
        #[arg(long = "no-human-like", action = clap::ArgAction::SetFalse)]
        human_like: bool,
    },
    /// Locate by accessible name / visible text and click
    #[command(name = "click_by_text", alias = "click-by-text")]
    ClickByText {
        #[command(flatten)]
        conn: Conn,
        /// Text to look for (case-insensitive substring)
        #[arg(long)]
        text: String,
        /// Seconds to wait for the text to appear
        #[arg(long, default_value_t = 10.0)]
        timeout: f64,
        /// Bare press/release at the box centre instead of the human-like actuator
        #[arg(long = "no-human-like", action = clap::ArgAction::SetFalse)]
        human_like: bool,
    },
    /// Type into the element a ref names
    #[command(name = "type_by_ref", alias = "type-by-ref")]
    TypeByRef {
        #[command(flatten)]
        conn: Conn,
        /// Element ref from page_discover
        #[arg(long = "ref")]
        r#ref: String,
        /// Text to type
        #[arg(long)]
        text: String,
        /// Clear existing content first
        #[arg(long)]
        clear: bool,
        /// Press Enter after typing
        #[arg(long)]
        enter: bool,
        /// Per-character key events (live filters / autocomplete)
        #[arg(long)]
        keystrokes: bool,
        /// Type character-by-character with human timing (anti-bot pages)
        #[arg(long)]
        human_like: bool,
    },
    /// Focus an element without clicking
    #[command(name = "focus_by_ref", alias = "focus-by-ref")]
    FocusByRef {
        #[command(flatten)]
        conn: Conn,
        /// Element ref from page_discover
        #[arg(long = "ref")]
        r#ref: String,
    },
    /// Move the cursor over an element (hover menus, tooltips)
    #[command(name = "hover_by_ref", alias = "hover-by-ref")]
    HoverByRef {
        #[command(flatten)]
        conn: Conn,
        /// Element ref from page_discover
        #[arg(long = "ref")]
        r#ref: String,
    },
    /// Draw a red overlay on an element for a few seconds
    #[command(name = "highlight_by_ref", alias = "highlight-by-ref")]
    HighlightByRef {
        #[command(flatten)]
        conn: Conn,
        /// Element ref from page_discover
        #[arg(long = "ref")]
        r#ref: String,
        /// Seconds to show the overlay
        #[arg(long, default_value_t = 2.0)]
        duration: f64,
    },
    /// outerHTML of one element
    #[command(name = "html_by_ref", alias = "html-by-ref")]
    HtmlByRef {
        #[command(flatten)]
        conn: Conn,
        /// Element ref from page_discover
        #[arg(long = "ref")]
        r#ref: String,
    },
    /// Screenshot just one element's box
    #[command(name = "screenshot_by_ref", alias = "screenshot-by-ref")]
    ScreenshotByRef {
        #[command(flatten)]
        conn: Conn,
        /// Element ref from page_discover
        #[arg(long = "ref")]
        r#ref: String,
        /// Output path (default $AI_DEV_BROWSER_OUTPUT_DIR or ./output/{timestamp}_element.png)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Per-call cap as JSON: '{"max_bytes": 200000, "max_dimension": 1024}'
        #[arg(long)]
        image_cap: Option<String>,
    },
    /// Select an <option> inside a native <select>
    #[command(name = "select_by_ref", alias = "select-by-ref")]
    SelectByRef {
        #[command(flatten)]
        conn: Conn,
        /// Ref of the <option>
        #[arg(long = "ref")]
        r#ref: String,
    },
    /// Set files on an <input type="file">
    #[command(name = "upload_by_ref", alias = "upload-by-ref")]
    UploadByRef {
        #[command(flatten)]
        conn: Conn,
        /// Ref of the file input
        #[arg(long = "ref")]
        r#ref: String,
        /// Comma-separated absolute file paths
        #[arg(long)]
        paths: String,
    },
    /// Press a key (Enter, Tab, Escape, Backspace, Delete, Space, arrows, Home, End, PageUp/Down)
    #[command(name = "press_key", alias = "press-key")]
    PressKey {
        #[command(flatten)]
        conn: Conn,
        /// Key name (case-insensitive; aliases esc, del, return, up/down/left/right)
        #[arg(long)]
        key: String,
        /// Focus this ref first
        #[arg(long = "ref")]
        r#ref: Option<String>,
        /// Modifier bitmask: 1=Alt, 2=Ctrl, 4=Meta, 8=Shift
        #[arg(long, default_value_t = 0)]
        modifiers: i64,
    },
    /// Locate by accessible name and report {found, ref, role, name, x, y}
    #[command(name = "find_by_text", alias = "find-by-text")]
    FindByText {
        #[command(flatten)]
        conn: Conn,
        /// Text to look for (case-insensitive substring)
        #[arg(long)]
        text: String,
        /// Only match buttons / links / inputs (no StaticText fallback)
        #[arg(long)]
        interactable_only: bool,
    },
    /// {found, tag, text, visible, attrs} for an html id (same-origin frames included)
    #[command(name = "find_by_html_id", alias = "find-by-html-id")]
    FindByHtmlId {
        #[command(flatten)]
        conn: Conn,
        /// Value of the id attribute
        #[arg(long)]
        html_id: String,
    },
    /// {found, tag, text, visible, attrs} for the first XPath match (same-origin frames included)
    #[command(name = "find_by_xpath", alias = "find-by-xpath")]
    FindByXpath {
        #[command(flatten)]
        conn: Conn,
        /// XPath expression
        #[arg(long)]
        xpath: String,
    },
    /// Trusted click on the element with an html id
    #[command(name = "click_by_html_id", alias = "click-by-html-id")]
    ClickByHtmlId {
        #[command(flatten)]
        conn: Conn,
        /// Value of the id attribute
        #[arg(long)]
        html_id: String,
    },
    /// Trusted click on the first XPath match
    #[command(name = "click_by_xpath", alias = "click-by-xpath")]
    ClickByXpath {
        #[command(flatten)]
        conn: Conn,
        /// XPath expression
        #[arg(long)]
        xpath: String,
    },
    /// Click / double-click a grid row by its text, or toggle its checkbox
    #[command(name = "click_row_by_text", alias = "click-row-by-text")]
    ClickRowByText {
        #[command(flatten)]
        conn: Conn,
        /// Text the row contains
        #[arg(long)]
        text: String,
        /// Double-click the row
        #[arg(long)]
        double: bool,
        /// 0-based index when several rows match
        #[arg(long, default_value_t = 0)]
        nth: i64,
        /// Toggle the row's checkbox instead of clicking the row
        #[arg(long)]
        checkbox: bool,
    },
    /// Locate an input by its label / placeholder / accessible name and type into it
    #[command(name = "type_by_text", alias = "type-by-text")]
    TypeByText {
        #[command(flatten)]
        conn: Conn,
        /// Accessible name of the input
        #[arg(long)]
        name: String,
        /// Text to type
        #[arg(long)]
        text: String,
        /// Clear existing content first
        #[arg(long)]
        clear: bool,
        /// Seconds to wait for the name to appear
        #[arg(long, default_value_t = 10.0)]
        timeout: f64,
        /// Human timing between keystrokes
        #[arg(long)]
        human_like: bool,
        /// Press Enter after typing
        #[arg(long)]
        enter: bool,
        /// Real per-character key events
        #[arg(long)]
        keystrokes: bool,
    },
    /// Build a real text selection over on-page text
    #[command(name = "select_text", alias = "select-text")]
    SelectText {
        #[command(flatten)]
        conn: Conn,
        /// Text to select (case-sensitive substring of one text node)
        #[arg(long)]
        text: String,
        /// Extend the selection to the end of this text
        #[arg(long)]
        to_text: Option<String>,
    },
    /// Drag the element a ref names to destination coordinates
    #[command(name = "drag_by_ref", alias = "drag-by-ref")]
    DragByRef {
        #[command(flatten)]
        conn: Conn,
        /// Element ref to drag from
        #[arg(long = "ref")]
        r#ref: String,
        /// Destination X (CSS px)
        #[arg(long)]
        to_x: f64,
        /// Destination Y (CSS px)
        #[arg(long)]
        to_y: f64,
        /// Intermediate move steps (linear drag)
        #[arg(long, default_value_t = 10)]
        steps: usize,
        /// Human-like drag: hold, gaussian path with timing, pause, release
        #[arg(long)]
        human_like: bool,
    },
    /// Move the cursor to coordinates without clicking
    #[command(name = "mouse_move", alias = "mouse-move")]
    MouseMove {
        #[command(flatten)]
        conn: Conn,
        /// X coordinate (screenshot space if --screenshot is given)
        #[arg(long)]
        x: f64,
        /// Y coordinate
        #[arg(long)]
        y: f64,
        /// Screenshot PNG whose embedded metadata maps image px → CSS px
        #[arg(long)]
        screenshot: Option<PathBuf>,
        /// Intermediate steps (native line)
        #[arg(long, default_value_t = 10)]
        steps: usize,
        /// Straight line instead of the gaussian path
        #[arg(long = "no-human-like", action = clap::ArgAction::SetFalse)]
        human_like: bool,
    },
    /// Click at raw coordinates (canvas / SVG / screenshot-derived)
    #[command(name = "mouse_click", alias = "mouse-click")]
    MouseClick {
        #[command(flatten)]
        conn: Conn,
        /// X coordinate (screenshot space if --screenshot is given)
        #[arg(long)]
        x: f64,
        /// Y coordinate
        #[arg(long)]
        y: f64,
        /// Screenshot PNG whose embedded metadata maps image px → CSS px
        #[arg(long)]
        screenshot: Option<PathBuf>,
        /// left / right / middle
        #[arg(long, default_value = "left")]
        button: String,
        /// Modifier bitmask: 1=Alt, 2=Ctrl, 4=Meta, 8=Shift
        #[arg(long, default_value_t = 0)]
        modifiers: i64,
        /// Double-click
        #[arg(long)]
        double: bool,
        /// Bare press/release instead of the human-like actuator
        #[arg(long = "no-human-like", action = clap::ArgAction::SetFalse)]
        human_like: bool,
        /// Skip the pre-click cursor move (fewest events; heavy SPAs)
        #[arg(long = "no-move", action = clap::ArgAction::SetFalse)]
        r#move: bool,
    },
    /// Drag from one coordinate to another
    #[command(name = "mouse_drag", alias = "mouse-drag")]
    MouseDrag {
        #[command(flatten)]
        conn: Conn,
        /// Start X (screenshot space if --screenshot is given)
        #[arg(long)]
        from_x: f64,
        /// Start Y
        #[arg(long)]
        from_y: f64,
        /// End X
        #[arg(long)]
        to_x: f64,
        /// End Y
        #[arg(long)]
        to_y: f64,
        /// Screenshot PNG whose embedded metadata maps image px → CSS px
        #[arg(long)]
        screenshot: Option<PathBuf>,
        /// Intermediate move steps (linear drag)
        #[arg(long, default_value_t = 10)]
        steps: usize,
        /// Human-like drag: hold, gaussian path with timing, pause, release
        #[arg(long)]
        human_like: bool,
    },
    /// List open tabs
    #[command(name = "tab_list", alias = "tab-list")]
    TabList {
        #[command(flatten)]
        conn: Conn,
    },
    /// Activate a tab by index
    #[command(name = "tab_switch", alias = "tab-switch")]
    TabSwitch {
        #[command(flatten)]
        conn: Conn,
        /// Tab index from tab_list
        #[arg(long)]
        tab_id: usize,
    },
    /// Open a new tab
    #[command(name = "tab_new", alias = "tab-new")]
    TabNew {
        #[command(flatten)]
        conn: Conn,
        /// URL to open (default about:blank)
        #[arg(long)]
        url: Option<String>,
    },
    /// Close a tab by index (never the last one)
    #[command(name = "tab_close", alias = "tab-close")]
    TabClose {
        #[command(flatten)]
        conn: Conn,
        /// Tab index from tab_list (default 0)
        #[arg(long)]
        tab_id: Option<usize>,
    },
    /// Evaluate JavaScript in the page (raw escape hatch)
    #[command(name = "js_evaluate", alias = "js-evaluate")]
    JsEvaluate {
        #[command(flatten)]
        conn: Conn,
        /// JavaScript expression; the last expression's value is returned
        #[arg(long)]
        expression: String,
        /// Run inside a cross-origin iframe: URL substring or target id
        #[arg(long)]
        frame: Option<String>,
    },
    /// Send a raw CDP command: --method Domain.command --params '{...}'
    #[command(name = "cdp_send", alias = "cdp-send")]
    CdpSend {
        #[command(flatten)]
        conn: Conn,
        /// CDP method, e.g. Browser.getVersion
        #[arg(long)]
        method: String,
        /// JSON object of parameters (snake_case or camelCase keys)
        #[arg(long)]
        params: Option<String>,
    },
    /// Accept / dismiss a JavaScript dialog (alert / confirm / prompt / beforeunload)
    #[command(name = "dialog_respond", alias = "dialog-respond")]
    DialogRespond {
        #[command(flatten)]
        conn: Conn,
        /// accept or dismiss
        #[arg(long, default_value = "accept")]
        action: String,
        /// Text for prompt() when accepting
        #[arg(long)]
        prompt_text: Option<String>,
        /// Auto-respond to all future dialogs on this tab
        #[arg(long)]
        auto_handle: bool,
        /// Seconds to wait for a dialog to appear
        #[arg(long, default_value_t = 0.0)]
        wait_timeout: f64,
    },
    /// Set the render viewport, OS window state, or focus
    #[command(name = "window_set", alias = "window-set")]
    WindowSet {
        #[command(flatten)]
        conn: Conn,
        /// Render viewport width (innerWidth)
        #[arg(long)]
        width: Option<u32>,
        /// Render viewport height (innerHeight)
        #[arg(long)]
        height: Option<u32>,
        /// normal / maximized / minimized / fullscreen (headed only)
        #[arg(long)]
        state: Option<String>,
        /// Bring the window to front (headed only)
        #[arg(long)]
        focus: bool,
    },
    /// Read localStorage (one key or all)
    #[command(name = "storage_get", alias = "storage-get")]
    StorageGet {
        #[command(flatten)]
        conn: Conn,
        /// Key to read (default: all)
        #[arg(long)]
        key: Option<String>,
    },
    /// Write localStorage (--items JSON, or --key/--value)
    #[command(name = "storage_set", alias = "storage-set")]
    StorageSet {
        #[command(flatten)]
        conn: Conn,
        /// JSON object of key/value pairs
        #[arg(long)]
        items: Option<String>,
        /// Single key
        #[arg(long)]
        key: Option<String>,
        /// Value for --key
        #[arg(long)]
        value: Option<String>,
    },
    /// List browser cookies
    #[command(name = "cookies_list", alias = "cookies-list")]
    CookiesList {
        #[command(flatten)]
        conn: Conn,
        /// Domain substring filter
        #[arg(long)]
        domain: Option<String>,
    },
    /// Save browser cookies to a JSON file
    #[command(name = "cookies_save", alias = "cookies-save")]
    CookiesSave {
        #[command(flatten)]
        conn: Conn,
        /// File path (default ~/.ai-dev-browser/cookies.dat)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Regex; only cookies whose JSON matches are saved
        #[arg(long)]
        pattern: Option<String>,
    },
    /// Load cookies from a JSON file into the browser
    #[command(name = "cookies_load", alias = "cookies-load")]
    CookiesLoad {
        #[command(flatten)]
        conn: Conn,
        /// File path (default ~/.ai-dev-browser/cookies.dat)
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Import cookies for a domain from the user's real browser (macOS/Linux)
    #[command(name = "cookies_import", alias = "cookies-import")]
    CookiesImport {
        #[command(flatten)]
        conn: Conn,
        /// Domain to import (e.g. ".grok.com")
        #[arg(long)]
        domain: String,
        /// Source browser: chrome / chromium / brave / edge
        #[arg(long, default_value = "chrome")]
        browser: String,
        /// Explicit Chrome user-data directory
        #[arg(long)]
        user_data_dir: Option<String>,
    },
    /// Open a visible browser for manual login, then export cookies on close
    #[command(name = "login_interactive", alias = "login-interactive")]
    LoginInteractive {
        /// Login page URL
        #[arg(long)]
        url: String,
        /// Where to save cookies (default ~/.ai-dev-browser/cookies.dat)
        #[arg(long)]
        cookies_path: Option<PathBuf>,
    },
    /// Extract + decrypt cookies for a domain (no automation browser)
    #[command(name = "cookies_extract", alias = "cookies-extract")]
    CookiesExtract {
        /// Domain to read (e.g. ".grok.com")
        #[arg(long)]
        domain: String,
        /// Source browser: chrome / chromium / brave / edge
        #[arg(long, default_value = "chrome")]
        browser: String,
        /// Explicit Chrome user-data directory
        #[arg(long)]
        user_data_dir: Option<String>,
    },
    /// Download a file by URL into a directory
    #[command(name = "download")]
    Download {
        #[command(flatten)]
        conn: Conn,
        /// Direct URL of the file
        #[arg(long)]
        url: String,
        /// Download directory (default ./downloads)
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Trusted-click a download link (XPath) and wait for the file
    #[command(name = "download_link", alias = "download-link")]
    DownloadLink {
        #[command(flatten)]
        conn: Conn,
        /// XPath of the link / button
        #[arg(long)]
        xpath: String,
        /// Directory to save into (default ./downloads)
        #[arg(long)]
        download_dir: Option<PathBuf>,
        /// Seconds to wait for completion
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
    },
    /// Ask the VLM where an element is on the page; returns a clickable CSS
    /// point (usable by `mouse_click --x --y`).
    #[command(name = "locate", alias = "vlm_locate")]
    Locate {
        #[command(flatten)]
        conn: Conn,
        /// Natural-language description of the element.
        #[arg(long)]
        find: String,
        #[arg(long)]
        model: Option<String>,
    },
    /// Ask the VLM a yes/no question about the page; returns {answer, yes}.
    #[command(name = "ask", alias = "vlm_ask")]
    Ask {
        #[command(flatten)]
        conn: Conn,
        #[arg(long)]
        question: String,
        #[arg(long)]
        model: Option<String>,
    },
}

/// Screenshot the active tab to a temp file (css-scaled, capped), returning
/// the PNG bytes and the image dims + `scale_factor` (CSS px per image px).
async fn vlm_screenshot(tab: &Tab) -> sudohand_browser::Result<(Vec<u8>, u32, u32, f64)> {
    let path = std::env::temp_dir().join(format!("suh_browser_vlm_{}.png", std::process::id()));
    let opts = ScreenshotOptions {
        path: Some(path.clone()),
        full_page: false,
        css_scale: true,
        max_long_edge: 1400,
        max_total_pixels: 0,
        image_cap: None,
    };
    let meta = sudohand_browser::page::page_screenshot(tab, &opts).await?;
    let width = meta.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
    let height = meta.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
    let sf = meta
        .get("scale_factor")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let png = std::fs::read(&path).map_err(sudohand_browser::Error::from)?;
    let _ = std::fs::remove_file(&path);
    Ok((png, width, height, sf))
}

async fn browser_and_tab(conn: &Conn) -> sudohand_browser::Result<(BrowserClient, Tab)> {
    let mut browser = connect_browser(None, conn.port).await?;
    let tab = get_active_tab(&mut browser, conn.tab_url.as_deref()).await?;
    Ok((browser, tab))
}

fn parse_overrides(raw: Option<&str>) -> sudohand_browser::Result<Vec<(String, Option<String>)>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let v: Value = serde_json::from_str(raw)?;
    let obj = v.as_object().ok_or_else(|| {
        sudohand_browser::Error::Invalid(
            "--override-default-args must be a JSON object".to_string(),
        )
    })?;
    Ok(obj
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                match v {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    other => Some(other.to_string()),
                },
            )
        })
        .collect())
}

async fn run_async(tool: Cmd) -> sudohand_browser::Result<Value> {
    match tool {
        Cmd::BrowserStart {
            port,
            headless,
            url,
            profile,
            temp,
            reuse,
            startup_timeout,
            extra_args,
            override_default_args,
            silent_stderr,
        } => {
            let opts = StartOptions {
                port,
                headless: headless.as_deref().map(Headless::parse),
                url,
                profile,
                temp,
                reuse: match reuse {
                    ReuseArg::None => Reuse::None,
                    ReuseArg::Any => Reuse::Any,
                },
                startup_timeout: Some(startup_timeout),
                extra_args,
                override_default_args: parse_overrides(override_default_args.as_deref())?,
                silent_stderr,
            };
            sudohand_browser::tools::browser_start(&opts).await
        }
        Cmd::BrowserStop { port, stop_all } => {
            sudohand_browser::tools::browser_stop(port, stop_all).await
        }
        Cmd::BrowserList { all_workspaces } => {
            sudohand_browser::tools::browser_list(all_workspaces).await
        }
        Cmd::PageGoto {
            conn,
            url,
            tab_new,
            wait,
        } => {
            let (mut browser, tab) = browser_and_tab(&conn).await?;
            let tab = if tab_new {
                browser.new_tab("about:blank").await?
            } else {
                tab
            };
            sudohand_browser::tools::page_goto(&tab, &url, wait).await
        }
        Cmd::PageDiscover {
            conn,
            text,
            interactable_only,
            include_coordinates,
            include_iframes,
            dom_scan,
            dom_limit,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let opts = DiscoverOptions {
                text,
                interactable_only,
                include_coordinates,
                include_iframes,
                dom_scan,
                dom_limit,
            };
            Ok(serde_json::to_value(
                sudohand_browser::tools::page_discover(&tab, &opts).await?,
            )?)
        }
        Cmd::PageScreenshot {
            conn,
            path,
            full_page,
            css_scale,
            max_long_edge,
            max_total_pixels,
            image_cap,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let opts = ScreenshotOptions {
                path,
                full_page,
                css_scale,
                max_long_edge,
                max_total_pixels,
                image_cap: image_cap.as_deref().map(ImageCap::from_json).transpose()?,
            };
            sudohand_browser::tools::page_screenshot(&tab, &opts).await
        }
        Cmd::Locate { conn, find, model } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let (png, width, height, sf) = vlm_screenshot(&tab).await?;
            let mut vlm = sudohand_vlm::DashScopeVlm::from_env()
                .map_err(|e| sudohand_browser::Error::Invalid(e.to_string()))?;
            if let Some(m) = model {
                vlm.locate_model = m;
            }
            let locate_model = vlm.locate_model.clone();
            let t0 = std::time::Instant::now();
            let find2 = find.clone();
            let n =
                tokio::task::spawn_blocking(move || sudohand_vlm::Vlm::locate(&vlm, &png, &find2))
                    .await
                    .map_err(|e| sudohand_browser::Error::Invalid(format!("vlm join: {e}")))?
                    .map_err(|e| sudohand_browser::Error::Invalid(e.to_string()))?;
            let ix = n.x / 1000.0 * f64::from(width);
            let iy = n.y / 1000.0 * f64::from(height);
            Ok(json!({
                "find": find, "model": locate_model,
                "normalized": [n.x, n.y],
                "image": {"x": ix, "y": iy},
                "point": {"x": ix * sf, "y": iy * sf},
                "ms": t0.elapsed().as_millis(),
            }))
        }
        Cmd::Ask {
            conn,
            question,
            model,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let (png, _w, _h, _sf) = vlm_screenshot(&tab).await?;
            let mut vlm = sudohand_vlm::DashScopeVlm::from_env()
                .map_err(|e| sudohand_browser::Error::Invalid(e.to_string()))?;
            if let Some(m) = model {
                vlm.ask_model = m;
            }
            let ask_model = vlm.ask_model.clone();
            let t0 = std::time::Instant::now();
            let question2 = question.clone();
            let answer =
                tokio::task::spawn_blocking(move || sudohand_vlm::Vlm::ask(&vlm, &png, &question2))
                    .await
                    .map_err(|e| sudohand_browser::Error::Invalid(format!("vlm join: {e}")))?
                    .map_err(|e| sudohand_browser::Error::Invalid(e.to_string()))?;
            Ok(json!({
                "question": question, "answer": answer,
                "yes": sudohand_vlm::is_yes(&answer),
                "model": ask_model, "ms": t0.elapsed().as_millis(),
            }))
        }
        Cmd::PageInfo { conn } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::page_info(&tab).await
        }
        Cmd::PageReload { conn, ignore_cache } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::page_reload(&tab, ignore_cache).await
        }
        Cmd::PageWaitUrl {
            conn,
            pattern,
            exact,
            timeout,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::page_wait_url(
                &tab,
                pattern.as_deref(),
                exact.as_deref(),
                timeout,
            )
            .await
        }
        Cmd::PageWaitElement {
            conn,
            text,
            selector,
            timeout,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::page_wait_element(
                &tab,
                text.as_deref(),
                selector.as_deref(),
                timeout,
            )
            .await
        }
        Cmd::PageScroll {
            conn,
            direction,
            amount,
            to_bottom,
            to_top,
            to_element,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let opts = ScrollOptions {
                direction,
                amount,
                to_bottom,
                to_top,
                to_element,
            };
            sudohand_browser::tools::page_scroll(&tab, &opts).await
        }
        Cmd::PagePdf {
            conn,
            path,
            landscape,
            print_background,
            scale,
            paper_width,
            paper_height,
            margin_top,
            margin_bottom,
            margin_left,
            margin_right,
            page_ranges,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let opts = PdfOptions {
                path,
                landscape,
                print_background,
                scale,
                paper_width,
                paper_height,
                margin_top,
                margin_bottom,
                margin_left,
                margin_right,
                page_ranges,
            };
            sudohand_browser::tools::page_pdf(&tab, &opts).await
        }
        Cmd::PageEmulateFocus { conn, enabled } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::page_emulate_focus(&tab, enabled).await
        }
        Cmd::FocusByRef { conn, r#ref } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::focus_by_ref(&tab, &r#ref).await
        }
        Cmd::HoverByRef { conn, r#ref } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::hover_by_ref(&tab, &r#ref).await
        }
        Cmd::HighlightByRef {
            conn,
            r#ref,
            duration,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::highlight_by_ref(&tab, &r#ref, duration).await
        }
        Cmd::HtmlByRef { conn, r#ref } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::html_by_ref(&tab, &r#ref).await
        }
        Cmd::ScreenshotByRef {
            conn,
            r#ref,
            path,
            image_cap,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let cap = image_cap.as_deref().map(ImageCap::from_json).transpose()?;
            sudohand_browser::tools::screenshot_by_ref(&tab, &r#ref, path.as_deref(), cap.as_ref())
                .await
        }
        Cmd::SelectByRef { conn, r#ref } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::select_by_ref(&tab, &r#ref).await
        }
        Cmd::UploadByRef { conn, r#ref, paths } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::upload_by_ref(&tab, &r#ref, &paths).await
        }
        Cmd::PressKey {
            conn,
            key,
            r#ref,
            modifiers,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::press_key(&tab, &key, r#ref.as_deref(), modifiers).await
        }
        Cmd::FindByText {
            conn,
            text,
            interactable_only,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::find_by_text(&tab, &text, interactable_only).await
        }
        Cmd::FindByHtmlId { conn, html_id } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::find_by_html_id(&tab, &html_id).await
        }
        Cmd::FindByXpath { conn, xpath } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::find_by_xpath(&tab, &xpath).await
        }
        Cmd::ClickByHtmlId { conn, html_id } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::click_by_html_id(&tab, &html_id).await
        }
        Cmd::ClickByXpath { conn, xpath } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::click_by_xpath(&tab, &xpath).await
        }
        Cmd::ClickRowByText {
            conn,
            text,
            double,
            nth,
            checkbox,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::click_row_by_text(&tab, &text, double, nth, checkbox).await
        }
        Cmd::TypeByText {
            conn,
            name,
            text,
            clear,
            timeout,
            human_like,
            enter,
            keystrokes,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let opts = TypeByTextOptions {
                clear,
                timeout,
                human_like: human_like.then_some(true),
                enter,
                keystrokes,
            };
            sudohand_browser::tools::type_by_text(&tab, &name, &text, opts).await
        }
        Cmd::SelectText {
            conn,
            text,
            to_text,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::select_text(&tab, &text, to_text.as_deref()).await
        }
        Cmd::TabNew { conn, url } => {
            let mut browser = connect_browser(None, conn.port).await?;
            sudohand_browser::tools::tab_new(&mut browser, url.as_deref()).await
        }
        Cmd::TabClose { conn, tab_id } => {
            let mut browser = connect_browser(None, conn.port).await?;
            sudohand_browser::tools::tab_close(&mut browser, tab_id).await
        }
        Cmd::CdpSend {
            conn,
            method,
            params,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::cdp_send(&tab, &method, params.as_deref()).await
        }
        Cmd::DialogRespond {
            conn,
            action,
            prompt_text,
            auto_handle,
            wait_timeout,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let opts = DialogOptions {
                action,
                prompt_text,
                auto_handle,
                wait_timeout,
            };
            sudohand_browser::tools::dialog_respond(&tab, &opts).await
        }
        Cmd::WindowSet {
            conn,
            width,
            height,
            state,
            focus,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::window_set(&tab, width, height, state.as_deref(), focus).await
        }
        Cmd::StorageGet { conn, key } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::storage_get(&tab, key.as_deref()).await
        }
        Cmd::StorageSet {
            conn,
            items,
            key,
            value,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let items: Option<serde_json::Map<String, Value>> = match items {
                Some(raw) => match serde_json::from_str::<Value>(&raw)? {
                    Value::Object(m) => Some(m),
                    _ => {
                        return Err(sudohand_browser::Error::Invalid(
                            "--items must be a JSON object".to_string(),
                        ));
                    }
                },
                None => None,
            };
            sudohand_browser::tools::storage_set(
                &tab,
                items.as_ref(),
                key.as_deref(),
                value.as_deref(),
            )
            .await
        }
        Cmd::CookiesList { conn, domain } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::cookies_list(&tab, domain.as_deref()).await
        }
        Cmd::CookiesSave {
            conn,
            path,
            pattern,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::cookies_save(&tab, path.as_deref(), pattern.as_deref()).await
        }
        Cmd::CookiesLoad { conn, path } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::cookies_load(&tab, path.as_deref()).await
        }
        Cmd::CookiesImport {
            conn,
            domain,
            browser,
            user_data_dir,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::cookies_import(
                &tab,
                &domain,
                &browser,
                user_data_dir.as_deref(),
            )
            .await
        }
        Cmd::LoginInteractive { url, cookies_path } => {
            sudohand_browser::tools::login_interactive(&url, cookies_path.as_deref()).await
        }
        Cmd::CookiesExtract {
            domain,
            browser,
            user_data_dir,
        } => {
            let cookies = sudohand_browser::tools::cookies_extract(
                &domain,
                &browser,
                user_data_dir.as_deref(),
            )?;
            Ok(json!({
                "count": cookies.len(),
                "cookies": cookies.iter().map(|c| json!({
                    "name": c.name, "value": c.value, "domain": c.domain, "path": c.path,
                    "secure": c.secure, "httpOnly": c.http_only, "expires": c.expires,
                })).collect::<Vec<_>>(),
            }))
        }
        Cmd::Download { conn, url, path } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::download(&tab, &url, path.as_deref()).await
        }
        Cmd::DownloadLink {
            conn,
            xpath,
            download_dir,
            timeout,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::download_link(&tab, &xpath, download_dir.as_deref(), timeout)
                .await
        }
        Cmd::PageWaitReady {
            conn,
            timeout,
            idle_time,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let ready = sudohand_browser::tools::page_wait_ready(&tab, timeout, idle_time).await;
            if ready {
                Ok(json!({"ready": true}))
            } else {
                Ok(json!({"error": "Operation failed", "ready": false}))
            }
        }
        Cmd::PageHtml { conn, outer } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::page_html(&tab, outer).await
        }
        Cmd::ClickByRef {
            conn,
            r#ref,
            human_like,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::click_by_ref(&tab, &r#ref, human_like).await
        }
        Cmd::ClickByText {
            conn,
            text,
            timeout,
            human_like,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::click_by_text(&tab, &text, timeout, human_like).await
        }
        Cmd::DragByRef {
            conn,
            r#ref,
            to_x,
            to_y,
            steps,
            human_like,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::drag_by_ref(&tab, &r#ref, to_x, to_y, steps, human_like).await
        }
        Cmd::MouseMove {
            conn,
            x,
            y,
            screenshot,
            steps,
            human_like,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let moved = sudohand_browser::tools::mouse_move(
                &tab,
                x,
                y,
                screenshot.as_deref(),
                steps,
                Some(human_like),
            )
            .await?;
            Ok(json!({"moved": moved}))
        }
        Cmd::MouseClick {
            conn,
            x,
            y,
            screenshot,
            button,
            modifiers,
            double,
            human_like,
            r#move,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let opts = ClickOptions {
                button,
                modifiers,
                double,
                human_like: Some(human_like),
                r#move,
            };
            let clicked =
                sudohand_browser::tools::mouse_click(&tab, x, y, screenshot.as_deref(), &opts)
                    .await?;
            Ok(json!({"clicked": clicked}))
        }
        Cmd::MouseDrag {
            conn,
            from_x,
            from_y,
            to_x,
            to_y,
            screenshot,
            steps,
            human_like,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            let dragged = sudohand_browser::tools::mouse_drag(
                &tab,
                (from_x, from_y),
                (to_x, to_y),
                screenshot.as_deref(),
                steps,
                human_like,
            )
            .await?;
            Ok(json!({"dragged": dragged}))
        }
        Cmd::TypeByRef {
            conn,
            r#ref,
            text,
            clear,
            enter,
            keystrokes,
            human_like,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::tools::type_by_ref(
                &tab,
                &r#ref,
                &text,
                TypeOptions {
                    clear,
                    enter,
                    keystrokes,
                    human_like,
                },
            )
            .await
        }
        Cmd::TabList { conn } => {
            let mut browser = connect_browser(None, conn.port).await?;
            sudohand_browser::tools::tab_list(&mut browser).await
        }
        Cmd::TabSwitch { conn, tab_id } => {
            let mut browser = connect_browser(None, conn.port).await?;
            sudohand_browser::tools::tab_switch(&mut browser, tab_id).await
        }
        Cmd::JsEvaluate {
            conn,
            expression,
            frame,
        } => {
            let (_b, tab) = browser_and_tab(&conn).await?;
            sudohand_browser::page::js_evaluate_in(&tab, &expression, frame.as_deref()).await
        }
    }
}

/// Entry from [`run`]: dispatch to the tool body, mapping its browser error
/// onto the shared envelope.
async fn run_flow(cmd: Cmd) -> sudohand_core::Result<Value> {
    run_async(cmd).await.map_err(sudohand_core::Error::from)
}

/// Entry point for `suh browser <tool>`: a current-thread runtime, the
/// tool's own JSON on success, and the sudohand error envelope on failure.
pub fn run(cmd: Cmd) -> sudohand_core::Result<Value> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| sudohand_core::Error::internal(format!("tokio: {e}")))?;
    rt.block_on(run_flow(cmd))
}
