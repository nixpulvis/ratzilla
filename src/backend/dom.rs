use std::{
    cell::Cell as StdCell,
    io::{Error as IoError, Result as IoResult},
    rc::Rc,
};

use ratatui::{
    backend::WindowSize,
    buffer::Cell,
    layout::{Position, Rect, Size},
    prelude::{backend::ClearType, Backend},
};
use web_sys::{window, Document, Element};

use unicode_width::UnicodeWidthStr;

use crate::{
    backend::{
        cell_sized::CellSized,
        event_callback::{
            create_mouse_event, EventCallback, MouseConfig, KEY_EVENT_TYPES, MOUSE_EVENT_TYPES,
        },
        utils::*,
    },
    error::Error,
    event::{KeyEvent, MouseEvent},
    render::WebEventHandler,
    CursorShape,
};

/// Default cell size used as a fallback when measurement fails.
const DEFAULT_CELL_SIZE: (f64, f64) = (10.0, 20.0);

/// Options for the [`DomBackend`].
#[derive(Debug, Default)]
pub struct DomBackendOptions {
    /// The element ID.
    grid_id: Option<String>,
    /// The element to draw in, instead of looking one up by ID.
    parent: Option<Element>,
    /// The cursor shape.
    cursor_shape: CursorShape,
}

impl DomBackendOptions {
    /// Constructs a new [`DomBackendOptions`].
    pub fn new(grid_id: Option<String>, cursor_shape: CursorShape) -> Self {
        Self {
            grid_id,
            parent: None,
            cursor_shape,
        }
    }

    /// Draws in the given element, which can be one `getElementById` can't
    /// find, like an element in a shadow root.
    pub fn parent(mut self, parent: Element) -> Self {
        self.parent = Some(parent);
        self
    }

    /// Returns the grid ID.
    ///
    /// - If the grid ID is not set, it returns `"grid"`.
    /// - If the grid ID is set, it returns the grid ID suffixed with
    ///     `"_ratzilla_grid"`.
    pub fn grid_id(&self) -> String {
        match &self.grid_id {
            Some(id) => format!("{id}_ratzilla_grid"),
            None => "grid".to_string(),
        }
    }

    /// Returns the [`CursorShape`].
    pub fn cursor_shape(&self) -> &CursorShape {
        &self.cursor_shape
    }
}

/// The part of the screen whose text can be selected, shared between a
/// [`DomBackend`] and whatever draws on it, which knows where its text is.
///
/// By default all of the screen can be selected. Once an area is set, only
/// the cells inside it can, up to the last non-blank cell of each line, so
/// borders, margins and other chrome are left out of a selection and of
/// what's copied.
#[derive(Debug, Clone, Default)]
pub struct SelectableArea(Rc<StdCell<Option<Rect>>>);

impl SelectableArea {
    /// Lets only the text in `area` be selected, or all of the screen with
    /// `None`. Takes effect on the next draw.
    pub fn set(&self, area: Option<Rect>) {
        self.0.set(area);
    }
}

/// DOM backend.
///
/// This backend uses the DOM to render the content to the screen.
///
/// In other words, it draws each line of [`Cell`]s as a `<pre>` element, with
/// a `<span>` for each run of cells that look the same. With the
/// `hyperlinks` feature, the spans of cells that share a hyperlink are
/// wrapped in an `<a>`.
pub struct DomBackend {
    /// Whether the backend has been initialized.
    initialized: bool,
    /// Size measured on resize, applied by the next draw.
    pending_resize: Rc<StdCell<Option<GridMeasurement>>>,
    /// Cells, as last drawn.
    cells: Vec<Cell>,
    /// The `<pre>` element of each line.
    lines: Vec<Element>,
    /// Grid element.
    grid: Element,
    /// The parent of the grid element.
    grid_parent: Element,
    /// Document.
    document: Document,
    /// Options.
    options: DomBackendOptions,
    /// Cursor position.
    cursor_position: Option<Position>,
    /// Whether the cursor is shown.
    cursor_shown: bool,
    /// Where the cursor is drawn, if anywhere.
    drawn_cursor: Option<Position>,
    /// The part of the screen whose text can be selected.
    selectable: SelectableArea,
    /// The selectable area as last drawn.
    drawn_selectable: Option<Rect>,
    /// Buffer size to pass to [`ratatui::Terminal`]
    size: Size,
    /// The size, shared with the mouse handler, which outlives a resize.
    shared_size: Rc<StdCell<Size>>,
    /// Measured cell dimensions in pixels (width, height).
    cell_size: (f64, f64),
    /// Resize event callback handler.
    _resize_callback: EventCallback<web_sys::Event>,
    /// Mouse event callback handler.
    mouse_callback: Option<DomMouseCallbackState>,
    /// Key event callback handler.
    key_callback: Option<EventCallback<web_sys::KeyboardEvent>>,
}

/// Cell and grid size measured on resize.
#[derive(Debug, Clone, Copy)]
struct GridMeasurement {
    /// Cell dimensions in pixels (width, height).
    cell_size: (f64, f64),
    /// Grid size in cells.
    size: Size,
}

/// Type alias for mouse event callback state.
type DomMouseCallbackState = EventCallback<web_sys::MouseEvent>;

impl std::fmt::Debug for DomBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomBackend")
            .field("initialized", &self.initialized)
            .field("pending_resize", &self.pending_resize)
            .field("cells", &format!("[{} cells]", self.cells.len()))
            .field("size", &self.size)
            .field("cell_size", &self.cell_size)
            .field("cursor_position", &self.cursor_position)
            .field("resize_callback", &"...")
            .field("mouse_callback", &self.mouse_callback.is_some())
            .field("key_callback", &self.key_callback.is_some())
            .finish()
    }
}

impl DomBackend {
    /// Constructs a new [`DomBackend`].
    pub fn new() -> Result<Self, Error> {
        Self::new_with_options(DomBackendOptions::default())
    }

    /// Constructs a new [`DomBackend`] and uses the given element ID for the grid.
    pub fn new_by_id(id: &str) -> Result<Self, Error> {
        Self::new_with_options(DomBackendOptions::new(
            Some(id.to_string()),
            CursorShape::default(),
        ))
    }

    /// Constructs a new [`DomBackend`] that draws in the given element.
    pub fn new_by_element(parent: Element) -> Result<Self, Error> {
        Self::new_with_options(DomBackendOptions::default().parent(parent))
    }

    /// Returns the [`SelectableArea`] of this backend, to set it while
    /// drawing.
    pub fn selectable_area(&self) -> SelectableArea {
        self.selectable.clone()
    }

    /// Set the [`CursorShape`].
    pub fn set_cursor_shape(mut self, shape: CursorShape) -> Self {
        self.options.cursor_shape = shape;
        self
    }

    /// Constructs a new [`DomBackend`] with the given options.
    pub fn new_with_options(options: DomBackendOptions) -> Result<Self, Error> {
        let window = window().ok_or(Error::UnableToRetrieveWindow)?;
        let document = window.document().ok_or(Error::UnableToRetrieveDocument)?;
        let grid_parent = match &options.parent {
            Some(parent) => parent.clone(),
            None => get_element_by_id_or_body(options.grid_id.as_ref())?,
        };
        let cell_size =
            Self::measure_cell_size(&document, &grid_parent).unwrap_or(DEFAULT_CELL_SIZE);
        let size = Self::calculate_size(&grid_parent, cell_size);

        // Measured here, not in draw: ratatui calls size() before draw.
        let pending_resize = Rc::new(StdCell::new(None));
        let resize_callback = EventCallback::new(window.clone(), Self::RESIZE_EVENT_TYPES, {
            let pending_resize = pending_resize.clone();
            let document = document.clone();
            let grid_parent = grid_parent.clone();
            let grid = format!("#{}", options.grid_id());
            move |_: web_sys::Event| {
                if let Ok(Some(_)) = grid_parent.query_selector(&grid) {
                    grid_parent.set_inner_html("");
                }
                let cell_size =
                    Self::measure_cell_size(&document, &grid_parent).unwrap_or(DEFAULT_CELL_SIZE);
                let size = Self::calculate_size(&grid_parent, cell_size);
                pending_resize.set(Some(GridMeasurement { cell_size, size }));
            }
        })?;

        let mut backend = Self {
            initialized: false,
            pending_resize,
            cells: vec![],
            lines: vec![],
            grid: document.create_element("div")?,
            grid_parent,
            options,
            document,
            cursor_position: None,
            cursor_shown: false,
            drawn_cursor: None,
            selectable: SelectableArea::default(),
            drawn_selectable: None,
            size,
            shared_size: Rc::new(StdCell::new(size)),
            cell_size,
            _resize_callback: resize_callback,
            mouse_callback: None,
            key_callback: None,
        };
        backend.reset_grid()?;
        Ok(backend)
    }

    /// Measures the pixel dimensions of a single terminal cell.
    ///
    /// Creates a temporary `<pre><span>` probe element that inherits the
    /// page's CSS (font-family, font-size, etc.), measures it with
    /// `getBoundingClientRect()`, then removes the probe.
    fn measure_cell_size(document: &Document, parent: &Element) -> Result<(f64, f64), Error> {
        let pre = document.create_element("pre")?;
        pre.set_attribute(
            "style",
            "margin: 0; padding: 0; border: 0; line-height: normal;",
        )?;
        let span = document.create_element("span")?;
        span.set_inner_html("\u{2588}");
        span.set_attribute("style", "display: inline-block; width: 1ch;")?;
        pre.append_child(&span)?;
        parent.append_child(&pre)?;

        let rect = span.get_bounding_client_rect();
        let width = rect.width();
        let height = rect.height();

        parent.remove_child(&pre)?;

        if width > 0.0 && height > 0.0 {
            Ok((width, height))
        } else {
            Ok(DEFAULT_CELL_SIZE)
        }
    }

    /// Calculates the grid size in cells based on the parent element's dimensions and cell size.
    fn calculate_size(parent: &Element, cell_size: (f64, f64)) -> Size {
        let rect = parent.get_bounding_client_rect();
        let (parent_w, parent_h) = (rect.width(), rect.height());

        // Fall back to window dimensions if the parent has no size
        // (e.g. empty <body> with no explicit height)
        let (w, h) = if parent_w > 0.0 && parent_h > 0.0 {
            (parent_w, parent_h)
        } else {
            let (ww, wh) = get_raw_window_size();
            (ww as f64, wh as f64)
        };

        Size::new((w / cell_size.0) as u16, (h / cell_size.1) as u16)
    }

    /// Resize event types.
    const RESIZE_EVENT_TYPES: &[&str] = &["resize"];

    /// Reset the grid and clear the cells.
    fn reset_grid(&mut self) -> Result<(), Error> {
        self.grid = self.document.create_element("div")?;
        self.grid.set_attribute("id", &self.options.grid_id())?;
        self.cells.clear();
        self.lines.clear();
        Ok(())
    }

    /// Pre-render a blank content to the screen.
    ///
    /// This function is called from [`draw`] once (or after a resize)
    /// to render the right number of cells to the screen.
    fn populate(&mut self) -> Result<(), Error> {
        let (width, height) = (self.size.width as usize, self.size.height as usize);
        self.cells = vec![Cell::default(); width * height];
        let line_height = format!("height: {}px;", self.cell_size.1);
        for y in 0..self.size.height {
            let pre = self.document.create_element("pre")?;
            pre.set_attribute("style", &line_height)?;
            self.grid.append_child(&pre)?;
            self.lines.push(pre);
            self.draw_line(y)?;
        }
        Ok(())
    }

    /// Draws line `y` from its cells, with a `<span>` for each run of cells
    /// that look the same. The cursor's cell gets one of its own, cells
    /// outside the selectable area are kept out of any selection, and with
    /// the `hyperlinks` feature, linked runs go inside an `<a>`.
    fn draw_line(&self, y: u16) -> Result<(), Error> {
        let line = &self.lines[y as usize];
        let width = self.size.width as usize;
        let cells = &self.cells[y as usize * width..][..width];
        let cursor = self
            .drawn_cursor
            .filter(|cursor| cursor.y == y)
            .map(|cursor| cursor.x as usize);
        #[cfg(feature = "hyperlinks")]
        let same_link = |a: &Cell, b: &Cell| a.hyperlink() == b.hyperlink();
        #[cfg(not(feature = "hyperlinks"))]
        let same_link = |_: &Cell, _: &Cell| true;
        #[cfg(feature = "hyperlinks")]
        let mut anchor: Option<(Element, &ratatui::buffer::Hyperlink)> = None;
        let in_area = |area: Rect| (area.top()..area.bottom()).contains(&y);
        let selectable = match self.drawn_selectable {
            None => 0..width,
            Some(area) if in_area(area) => {
                let from = (area.left() as usize).min(width);
                let to = (area.right() as usize).min(width);
                let blank = |x: &usize| cells[*x].symbol().trim().is_empty();
                from..(from..to).rev().find(|x| !blank(x)).map_or(from, |x| x + 1)
            }
            Some(_) => 0..0,
        };

        line.set_text_content(None);
        let mut x = 0;
        while x < width {
            let start = x;
            let mut text = String::new();
            let mut columns = 0;
            while x < width
                && (x == start
                    || (looks_alike(&cells[start], &cells[x])
                        && same_link(&cells[start], &cells[x])
                        && cursor != Some(start)
                        && cursor != Some(x)
                        && selectable.contains(&start) == selectable.contains(&x)))
            {
                let symbol = cells[x].symbol();
                text.push_str(symbol);
                columns += symbol.width();
                // A wide glyph covers the next cell too.
                x += symbol.width().max(1);
            }

            let mut style = get_run_style_as_css(&cells[start], columns);
            if !selectable.contains(&start) {
                style.push_str(" user-select: none; -webkit-user-select: none;");
            }
            let span = create_span(&self.document, &text, &style)?;
            if cursor == Some(start) {
                update_css_field(self.options.cursor_shape.get_css_attribute(), &span)?;
            }

            #[cfg(feature = "hyperlinks")]
            match cells[start].hyperlink() {
                None => {
                    anchor = None;
                    line.append_child(&span)?;
                }
                Some(hyperlink) => match &anchor {
                    Some((elem, current)) if *current == hyperlink => {
                        elem.append_child(&span)?;
                    }
                    _ => {
                        let elem = create_anchor(&self.document, hyperlink)?;
                        elem.append_child(&span)?;
                        line.append_child(&elem)?;
                        anchor = Some((elem, hyperlink));
                    }
                },
            }
            #[cfg(not(feature = "hyperlinks"))]
            line.append_child(&span)?;
        }
        // Otherwise a blank line in the area is left out of a copy.
        if selectable.is_empty() && self.drawn_selectable.is_some_and(in_area) {
            let br = self.document.create_element("br")?;
            line.append_child(&br)?;
        }
        Ok(())
    }

    /// Redraws the lines the cursor moves between, when it's moved, shown
    /// or hidden.
    fn draw_cursor(&mut self) -> Result<(), Error> {
        let cursor = self.cursor_position.filter(|cursor| {
            self.cursor_shown && cursor.x < self.size.width && cursor.y < self.size.height
        });
        if !self.initialized || cursor == self.drawn_cursor {
            return Ok(());
        }
        let drawn = std::mem::replace(&mut self.drawn_cursor, cursor);
        for y in [drawn, cursor].into_iter().flatten().map(|cursor| cursor.y) {
            self.draw_line(y)?;
        }
        Ok(())
    }
}

impl CellSized for DomBackend {
    fn cell_size_px(&self) -> (f32, f32) {
        let dpr = get_device_pixel_ratio();
        (self.cell_size.0 as f32 * dpr, self.cell_size.1 as f32 * dpr)
    }

    fn cell_size_css_px(&self) -> (f32, f32) {
        (self.cell_size.0 as f32, self.cell_size.1 as f32)
    }
}

impl Backend for DomBackend {
    type Error = IoError;

    /// Draw the new content to the screen.
    ///
    /// This function is called in the [`ratatui::Terminal::flush`] function.
    /// This function recreate the DOM structure when it gets a resize event.
    fn draw<'a, I>(&mut self, content: I) -> IoResult<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        // Only runs on resize event.
        if let Some(measured) = self.pending_resize.take() {
            self.cell_size = measured.cell_size;
            if measured.size == self.size {
                // ratatui redraws only changed cells at an unchanged size, so
                // put back the old grid, which the resize listener detached.
                let line_height = format!("height: {}px;", self.cell_size.1);
                let mut row = self.grid.first_element_child();
                while let Some(line) = row {
                    line.set_attribute("style", &line_height)
                        .map_err(Error::from)?;
                    row = line.next_element_sibling();
                }
                if self.initialized {
                    self.grid_parent
                        .append_child(&self.grid)
                        .map_err(Error::from)?;
                }
            } else {
                self.size = measured.size;
                self.shared_size.set(measured.size);
                self.reset_grid()?;
                self.initialized = false;
            }
        }

        if !self.initialized {
            self.initialized = true;

            // Clear cursor position to avoid modifying css style of a non-existent cell
            self.cursor_position = None;
            self.drawn_cursor = None;

            self.grid_parent
                .append_child(&self.grid)
                .map_err(Error::from)?;
            self.populate()?;
        }

        // Redraw each changed line once, however many of its cells changed,
        // or every line when the selectable area has moved.
        let selectable = self.selectable.0.get();
        let moved = selectable != self.drawn_selectable;
        self.drawn_selectable = selectable;
        let mut changed = vec![moved; self.lines.len()];
        for (x, y, cell) in content {
            self.cells[(y * self.size.width + x) as usize] = cell.clone();
            changed[y as usize] = true;
        }
        for (y, _) in changed.iter().enumerate().filter(|(_, changed)| **changed) {
            self.draw_line(y as u16)?;
        }

        Ok(())
    }

    /// This function is called after the [`DomBackend::draw`] function.
    ///
    /// This function does nothing because the content is directly
    /// displayed by the draw function.
    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }

    fn hide_cursor(&mut self) -> IoResult<()> {
        self.cursor_shown = false;
        Ok(self.draw_cursor()?)
    }

    fn show_cursor(&mut self) -> IoResult<()> {
        self.cursor_shown = true;
        Ok(self.draw_cursor()?)
    }

    fn get_cursor(&mut self) -> IoResult<(u16, u16)> {
        Ok((0, 0))
    }

    fn set_cursor(&mut self, _x: u16, _y: u16) -> IoResult<()> {
        Ok(())
    }

    fn clear(&mut self) -> IoResult<()> {
        Ok(())
    }

    fn size(&self) -> IoResult<Size> {
        Ok(self
            .pending_resize
            .get()
            .map_or(self.size, |measured| measured.size))
    }

    fn window_size(&mut self) -> IoResult<WindowSize> {
        let (size, cell_size) = self
            .pending_resize
            .get()
            .map_or((self.size, self.cell_size), |measured| {
                (measured.size, measured.cell_size)
            });
        Ok(WindowSize {
            columns_rows: size,
            pixels: Size::new(
                (size.width as f64 * cell_size.0) as u16,
                (size.height as f64 * cell_size.1) as u16,
            ),
        })
    }

    fn get_cursor_position(&mut self) -> IoResult<Position> {
        match self.cursor_position {
            None => Ok((0, 0).into()),
            Some(position) => Ok(position),
        }
    }

    /// Update cursor_position and last_cursor_position
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> IoResult<()> {
        self.cursor_position = Some(position.into());
        Ok(self.draw_cursor()?)
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        match clear_type {
            ClearType::All => self.clear(),
            _ => Err(IoError::other("unimplemented")),
        }
    }
}

impl WebEventHandler for DomBackend {
    fn on_mouse_event<F>(&mut self, mut callback: F) -> Result<(), Error>
    where
        F: FnMut(MouseEvent) + 'static,
    {
        // Clear any existing handlers first
        self.clear_mouse_events();

        // A resize replaces the grid element, so listen on its parent and
        // look up the grid, and its size, when an event comes in.
        let parent = self.grid_parent.clone();
        let selector = format!("#{}", self.options.grid_id());
        let size = self.shared_size.clone();

        // Create mouse event callback
        let mouse_callback = EventCallback::new(
            self.grid_parent.clone(),
            MOUSE_EVENT_TYPES,
            move |event: web_sys::MouseEvent| {
                let Ok(Some(grid)) = parent.query_selector(&selector) else {
                    return;
                };
                // Cell dimensions are derived from element dimensions / grid size
                let config = MouseConfig::new(size.get().width, size.get().height);
                let mouse_event = create_mouse_event(&event, &grid, &config);
                callback(mouse_event);
            },
        )?;

        self.mouse_callback = Some(mouse_callback);

        Ok(())
    }

    fn clear_mouse_events(&mut self) {
        self.mouse_callback = None;
    }

    fn on_key_event<F>(&mut self, mut callback: F) -> Result<(), Error>
    where
        F: FnMut(KeyEvent) + 'static,
    {
        // Clear any existing handlers first
        self.clear_key_events();

        // Make the grid's parent focusable so it can receive key events. The
        // grid itself is replaced on a resize, which would lose the listener
        // and the focus with it.
        self.grid_parent.set_attribute("tabindex", "0")?;

        self.key_callback = Some(EventCallback::new(
            self.grid_parent.clone(),
            KEY_EVENT_TYPES,
            move |event: web_sys::KeyboardEvent| {
                callback(event.into());
            },
        )?);

        Ok(())
    }

    fn clear_key_events(&mut self) {
        self.key_callback = None;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ratatui::{widgets::Block, Terminal};
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    /// Appends a parent of a fixed size, so the grid doesn't depend on the window.
    fn sized_parent(id: &str, width: u32, height: u32) -> Element {
        let document = window().unwrap().document().unwrap();
        let parent = document.create_element("div").unwrap();
        parent.set_id(id);
        set_parent_size(&parent, width, height);
        document.body().unwrap().append_child(&parent).unwrap();
        parent
    }

    fn set_parent_size(parent: &Element, width: u32, height: u32) {
        parent
            .set_attribute("style", &format!("width: {width}px; height: {height}px;"))
            .unwrap();
    }

    /// The rows and columns of the grid in the DOM.
    fn dom_grid_size(parent: &Element) -> Size {
        let grid = parent.first_element_child().unwrap();
        let rows = grid.child_element_count();
        let line = grid.first_element_child().unwrap().text_content().unwrap();
        Size::new(line.chars().count() as u16, rows as u16)
    }

    fn fire_resize() {
        let resize = web_sys::Event::new("resize").unwrap();
        window().unwrap().dispatch_event(&resize).unwrap();
    }

    fn draw(terminal: &mut Terminal<DomBackend>) {
        terminal
            .draw(|frame| frame.render_widget(Block::bordered(), frame.area()))
            .unwrap();
    }

    #[cfg(feature = "hyperlinks")]
    #[wasm_bindgen_test]
    fn hyperlinks_are_anchors() {
        use ratatui::{buffer::Hyperlink, layout::Rect, text::Line};

        let parent = sized_parent("hyperlinks_are_anchors", 300, 200);
        let mut terminal =
            Terminal::new(DomBackend::new_by_id("hyperlinks_are_anchors").unwrap()).unwrap();
        let link = Hyperlink::new("https://ratatui.rs").with_id("a");
        terminal
            .draw(|frame| {
                frame.render_widget(Line::from("go ratatui now"), frame.area());
                frame
                    .buffer_mut()
                    .set_hyperlink(Rect::new(3, 0, 7, 1), Some(&link));
            })
            .unwrap();
        let line = parent
            .first_element_child()
            .unwrap()
            .first_element_child()
            .unwrap();
        let mut anchors = 0;
        let mut child = line.first_element_child();
        while let Some(elem) = child {
            anchors += usize::from(elem.tag_name() == "A");
            child = elem.next_element_sibling();
        }
        assert_eq!(anchors, 1);
        let anchor = line.query_selector("a").unwrap().unwrap();
        assert_eq!(anchor.get_attribute("href").unwrap(), "https://ratatui.rs");
        assert_eq!(anchor.text_content().unwrap(), "ratatui");
        assert_eq!(line.text_content().unwrap().trim_end(), "go ratatui now");

        // Taking the link away unwraps the cells again.
        terminal
            .draw(|frame| frame.render_widget(Line::from("go ratatui now"), frame.area()))
            .unwrap();
        let line = parent
            .first_element_child()
            .unwrap()
            .first_element_child()
            .unwrap();
        assert!(line.query_selector("a").unwrap().is_none());
        assert_eq!(line.text_content().unwrap().trim_end(), "go ratatui now");
    }

    #[wasm_bindgen_test]
    fn draws_in_a_shadow_root() {
        let host = sized_parent("draws_in_a_shadow_root", 300, 200);
        let shadow = host
            .attach_shadow(&web_sys::ShadowRootInit::new(web_sys::ShadowRootMode::Open))
            .unwrap();
        let document = window().unwrap().document().unwrap();
        let parent = document.create_element("div").unwrap();
        set_parent_size(&parent, 300, 200);
        shadow.append_child(&parent).unwrap();

        let mut terminal =
            Terminal::new(DomBackend::new_by_element(parent.clone()).unwrap()).unwrap();
        draw(&mut terminal);
        assert_eq!(terminal.size().unwrap(), dom_grid_size(&parent));

        // A resize still replaces the grid rather than adding another.
        set_parent_size(&parent, 200, 100);
        fire_resize();
        draw(&mut terminal);
        assert_eq!(parent.child_element_count(), 1);
        assert_eq!(terminal.size().unwrap(), dom_grid_size(&parent));
    }

    #[wasm_bindgen_test]
    fn size_matches_grid() {
        let parent = sized_parent("size_matches_grid", 300, 200);
        let mut terminal =
            Terminal::new(DomBackend::new_by_id("size_matches_grid").unwrap()).unwrap();
        draw(&mut terminal);
        assert_eq!(terminal.size().unwrap(), dom_grid_size(&parent));
    }

    #[wasm_bindgen_test]
    fn size_matches_grid_after_resize() {
        let parent = sized_parent("size_matches_grid_after_resize", 300, 200);
        let mut terminal =
            Terminal::new(DomBackend::new_by_id("size_matches_grid_after_resize").unwrap())
                .unwrap();
        draw(&mut terminal);
        let before = terminal.size().unwrap();

        set_parent_size(&parent, 200, 100);
        fire_resize();
        draw(&mut terminal);

        assert_ne!(terminal.size().unwrap(), before);
        assert_eq!(terminal.size().unwrap(), dom_grid_size(&parent));
    }

    #[wasm_bindgen_test]
    fn mouse_events_after_resize() {
        let parent = sized_parent("mouse_events_after_resize", 300, 200);
        let mut terminal =
            Terminal::new(DomBackend::new_by_id("mouse_events_after_resize").unwrap()).unwrap();
        let events = Rc::new(StdCell::new(0));
        let count = events.clone();
        terminal
            .backend_mut()
            .on_mouse_event(move |_| count.set(count.get() + 1))
            .unwrap();
        draw(&mut terminal);

        // A new size replaces the grid element.
        set_parent_size(&parent, 400, 300);
        fire_resize();
        draw(&mut terminal);

        let grid = parent.first_element_child().unwrap();
        let init = web_sys::MouseEventInit::new();
        init.set_bubbles(true);
        let event =
            web_sys::MouseEvent::new_with_mouse_event_init_dict("mousedown", &init).unwrap();
        grid.dispatch_event(&event).unwrap();
        assert_eq!(events.get(), 1);
    }

    #[wasm_bindgen_test]
    fn key_events_after_resize() {
        let parent = sized_parent("key_events_after_resize", 300, 200);
        let mut terminal =
            Terminal::new(DomBackend::new_by_id("key_events_after_resize").unwrap()).unwrap();
        let events = Rc::new(StdCell::new(0));
        let count = events.clone();
        terminal
            .backend_mut()
            .on_key_event(move |_| count.set(count.get() + 1))
            .unwrap();
        draw(&mut terminal);

        // A new size replaces the grid element.
        set_parent_size(&parent, 400, 300);
        fire_resize();
        draw(&mut terminal);

        let grid = parent.first_element_child().unwrap();
        let init = web_sys::KeyboardEventInit::new();
        init.set_bubbles(true);
        init.set_key("j");
        let event =
            web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init).unwrap();
        grid.dispatch_event(&event).unwrap();
        assert_eq!(events.get(), 1);
    }

    #[wasm_bindgen_test]
    fn draws_alike_cells_as_one_span() {
        let parent = sized_parent("draws_alike_cells_as_one_span", 300, 200);
        let mut terminal =
            Terminal::new(DomBackend::new_by_id("draws_alike_cells_as_one_span").unwrap()).unwrap();
        draw(&mut terminal);
        let grid = parent.first_element_child().unwrap();
        // The top border is all one style.
        assert_eq!(grid.first_element_child().unwrap().child_element_count(), 1);
    }

    #[wasm_bindgen_test]
    fn selects_only_the_selectable_area() {
        use ratatui::text::Line;

        let parent = sized_parent("selects_only_the_selectable_area", 300, 200);
        let mut terminal =
            Terminal::new(DomBackend::new_by_id("selects_only_the_selectable_area").unwrap())
                .unwrap();
        terminal
            .backend()
            .selectable_area()
            .set(Some(Rect::new(1, 1, 20, 2)));
        terminal
            .draw(|frame| {
                frame.render_widget(Block::bordered(), frame.area());
                frame.render_widget(Line::from("hi there"), Rect::new(1, 1, 20, 1));
            })
            .unwrap();

        let selectable = |line: Element| {
            let mut text = String::new();
            let mut span = line.first_element_child();
            while let Some(elem) = span {
                let style = elem.get_attribute("style").unwrap();
                if !style.contains("user-select: none") {
                    text.push_str(&elem.text_content().unwrap());
                }
                span = elem.next_element_sibling();
            }
            text
        };
        let grid = parent.first_element_child().unwrap();
        let top = grid.first_element_child().unwrap();
        assert_eq!(selectable(top.clone()), "");
        let text = top.next_element_sibling().unwrap();
        assert_eq!(selectable(text.clone()), "hi there");
        let blank = text.next_element_sibling().unwrap();
        assert_eq!(blank.last_element_child().unwrap().tag_name(), "BR");
    }

    #[wasm_bindgen_test]
    fn keeps_content_after_same_size_resize() {
        let parent = sized_parent("keeps_content_after_same_size_resize", 300, 200);
        let mut terminal =
            Terminal::new(DomBackend::new_by_id("keeps_content_after_same_size_resize").unwrap())
                .unwrap();
        draw(&mut terminal);
        let drawn = parent.text_content().unwrap();

        fire_resize();
        draw(&mut terminal);

        assert_eq!(parent.text_content().unwrap(), drawn);
    }

    #[wasm_bindgen_test]
    fn rows_follow_cell_height_after_same_size_resize() {
        // Sized in `em`, so a bigger font gives bigger cells but the same grid.
        let parent = sized_parent("rows_follow_cell_height", 0, 0);
        let style = |font: u32| format!("font-size: {font}px; width: 30em; height: 20em;");
        parent.set_attribute("style", &style(16)).unwrap();
        let mut terminal =
            Terminal::new(DomBackend::new_by_id("rows_follow_cell_height").unwrap()).unwrap();
        draw(&mut terminal);
        let size = terminal.size().unwrap();
        let row_height = || {
            let grid = parent.first_element_child().unwrap();
            grid.first_element_child()
                .unwrap()
                .get_bounding_client_rect()
                .height() as f32
        };
        let before = row_height();

        parent.set_attribute("style", &style(24)).unwrap();
        fire_resize();
        draw(&mut terminal);

        assert_eq!(terminal.size().unwrap(), size);
        assert_ne!(row_height(), before);
        assert_eq!(row_height(), terminal.backend().cell_size_css_px().1);
    }
}
