//! WebGL2 context creation and capability probe (#17).
//!
//! D-010 puts the context here and nowhere else: `hane-gpu` decides *what* to
//! draw as plain data, this crate holds the `WebGl2RenderingContext` and does
//! the drawing. So the probe -- the one piece of P2 that is nothing but GL
//! calls -- lives here too.
//!
//! # Why this runs before anything renders
//!
//! A renderer that discovers halfway through a frame that it cannot make a
//! float render target has already cleared the canvas, and the user sees a
//! blank artboard with a message in a console they will never open. Probing
//! first turns "hane is broken" into "hane needs `EXT_color_buffer_float`,
//! which this browser does not have".
//!
//! # The first `getContext` call wins
//!
//! Calling `getContext("webgl2")` twice on one canvas returns the *same*
//! context both times and silently ignores the second call's attributes. So the
//! attributes chosen in [`context_attributes`] are the ones the renderer gets,
//! whether or not it agrees with them -- picking them here is not a detail, it
//! is the decision.

use std::cell::{Cell, RefCell};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{HtmlCanvasElement, WebGl2RenderingContext as Gl};

/// Extensions the renderer cannot start without.
///
/// `EXT_color_buffer_float` makes float and half-float textures *renderable*.
/// Sampling them is core WebGL2; rendering into them is not, and a tiled
/// rasterizer accumulates coverage into an offscreen target it renders to. On
/// a machine without it there is no fallback short of an 8-bit coverage buffer,
/// which is a different renderer, so this is a hard requirement and not a
/// quality setting.
///
/// Deliberately one entry. `EXT_float_blend` is reported but not required: it
/// is only needed to blend into a 32-bit float target, and 16-bit half-float --
/// blendable in core WebGL2 -- carries 10 bits of coverage mantissa, which is
/// four times what an 8-bit output can show. #19 chooses; the probe reports
/// enough for it to choose.
const REQUIRED: &[&str] = &["EXT_color_buffer_float"];

thread_local! {
    /// The probe result, so a second caller gets the answer without creating a
    /// second context or writing a second line to the console.
    static PROBE: RefCell<Option<GlProbe>> = const { RefCell::new(None) };
    /// Set by the `webglcontextlost` listener. A `Cell` and not a field of
    /// [`GlProbe`] because the probe is handed out by value and this changes
    /// after the fact.
    static CONTEXT_LOST: Cell<bool> = const { Cell::new(false) };
}

/// What this machine's WebGL2 implementation can do.
///
/// Every field is read once at startup. Nothing here changes for the life of
/// the page -- except context loss, which is [`hane_gl_context_lost`].
#[wasm_bindgen]
#[derive(Clone, Debug)]
pub struct GlProbe {
    /// `GL_MAX_TEXTURE_SIZE`: the largest texture edge, in texels.
    ///
    /// The floor of the spec is 2048, which is not enough to hold a 4K artboard
    /// in one target -- so this number decides whether the renderer can use a
    /// single offscreen buffer or has to split the canvas.
    pub max_texture_size: i32,
    /// `GL_MAX_DRAW_BUFFERS`: how many colour attachments one draw can write.
    ///
    /// Guaranteed to be at least 4 in WebGL2. More means coverage and colour
    /// can be produced in one pass instead of two.
    pub max_draw_buffers: i32,
    /// `EXT_color_buffer_float`: float and half-float textures are renderable.
    pub float_render_targets: bool,
    /// `EXT_float_blend`: blending works into a 32-bit float target.
    pub float_blend: bool,
    /// `OES_texture_float_linear`: float textures filter linearly rather than
    /// only by nearest neighbour.
    pub float_linear_filter: bool,
    /// Names from [`REQUIRED`] this implementation does not have.
    missing: Vec<&'static str>,
}

#[wasm_bindgen]
impl GlProbe {
    /// Whether the renderer can run at all: every required extension present.
    #[wasm_bindgen(getter)]
    pub fn supported(&self) -> bool {
        self.missing.is_empty()
    }

    /// The missing required extensions, comma-separated, or empty if none are.
    ///
    /// A string rather than an array because it exists to be put in front of a
    /// person, and `Vec<&'static str>` does not cross the boundary without
    /// allocating a `js_sys::Array` nobody would iterate.
    #[wasm_bindgen(getter)]
    pub fn missing_extensions(&self) -> String {
        self.missing.join(", ")
    }

    /// One line, for the console and for the benchmark page's header.
    pub fn summary(&self) -> String {
        let float = match (
            self.float_render_targets,
            self.float_blend,
            self.float_linear_filter,
        ) {
            (false, _, _) => "float: none",
            (true, true, true) => "float: render+blend+linear",
            (true, true, false) => "float: render+blend",
            (true, false, true) => "float: render+linear",
            (true, false, false) => "float: render",
        };
        let verdict = if self.supported() {
            String::from("ok")
        } else {
            format!("UNSUPPORTED, missing {}", self.missing_extensions())
        };
        format!(
            "hane webgl2: max texture {}, max draw buffers {}, {float} -- {verdict}",
            self.max_texture_size, self.max_draw_buffers
        )
    }
}

/// Creates the WebGL2 context on `canvas`, probes it, and logs the result once.
///
/// Safe to call repeatedly: the first call does the work and every later one
/// returns the same answer, which is what makes the result "available to the
/// benchmark page" without the page having to be the thing that probed.
///
/// Returns `Err` only when there is no context to probe -- WebGL2 unavailable
/// or disabled. A context that exists but is missing a required extension is
/// `Ok`, with [`GlProbe::supported`] false: that is a report, not a crash, and
/// the caller decides what to tell the user.
#[wasm_bindgen]
pub fn hane_gl_probe(canvas: &HtmlCanvasElement) -> Result<GlProbe, JsValue> {
    if let Some(cached) = PROBE.with(|p| p.borrow().clone()) {
        return Ok(cached);
    }

    let gl = context(canvas)?;

    watch_for_context_loss(canvas)?;

    let probe = GlProbe {
        max_texture_size: get_int(&gl, Gl::MAX_TEXTURE_SIZE),
        max_draw_buffers: get_int(&gl, Gl::MAX_DRAW_BUFFERS),
        float_render_targets: has_extension(&gl, "EXT_color_buffer_float"),
        float_blend: has_extension(&gl, "EXT_float_blend"),
        float_linear_filter: has_extension(&gl, "OES_texture_float_linear"),
        missing: REQUIRED
            .iter()
            .copied()
            .filter(|name| !has_extension(&gl, name))
            .collect(),
    };

    // Once, here, rather than leaving it to the shell: two callers must not
    // produce two lines, and the shell is not the only caller.
    let line = JsValue::from_str(&probe.summary());
    if probe.supported() {
        web_sys::console::log_1(&line);
    } else {
        web_sys::console::error_1(&line);
    }

    PROBE.with(|p| *p.borrow_mut() = Some(probe.clone()));
    Ok(probe)
}

/// Whether the GPU has taken the context away since the probe ran.
///
/// The browser fires `webglcontextlost` when the driver resets, the tab is
/// backgrounded on a memory-tight device, or another page hogs the GPU. Every
/// GL object made before that point is dead, and every call against them
/// silently does nothing -- which is exactly how a renderer ends up drawing a
/// blank canvas and reporting no error at all. Poll this and say so instead.
#[wasm_bindgen]
pub fn hane_gl_context_lost() -> bool {
    CONTEXT_LOST.with(Cell::get)
}

/// The one `getContext` in the crate.
///
/// Every caller goes through here, because the *first* call decides the
/// attributes for the life of the canvas and a second one asking for something
/// else would be silently ignored -- so the renderer (#19) must not have its
/// own, or it would inherit whatever ran first and never know.
pub(crate) fn context(canvas: &HtmlCanvasElement) -> Result<Gl, JsValue> {
    canvas
        .get_context_with_context_options("webgl2", &context_attributes())?
        .ok_or_else(|| JsValue::from_str("this browser has no WebGL2 (D-003 needs it)"))?
        .dyn_into::<Gl>()
        .map_err(Into::into)
}

/// The context attributes the whole renderer then lives with; see the module
/// docs on why this call is the one that decides them.
fn context_attributes() -> JsValue {
    let opts = js_sys::Object::new();
    let set = |k: &str, v: JsValue| {
        // Setting a property on an object literal we just made cannot fail.
        let _ = js_sys::Reflect::set(&opts, &JsValue::from_str(k), &v);
    };
    // The renderer computes its own analytic coverage. MSAA on the default
    // framebuffer would add a second, differently-quantised antialiasing on top
    // and put the output permanently out of reach of the CPU oracle (D-002).
    set("antialias", JsValue::FALSE);
    // 2D vector art has no depth and needs no stencil, and asking for them
    // costs a full-resolution buffer each.
    set("depth", JsValue::FALSE);
    set("stencil", JsValue::FALSE);
    // Premultiplied alpha over a transparent canvas, matching the `Pixmap`
    // contract the oracle's goldens are stored in, so a screenshot and a golden
    // are comparable without a conversion that loses precision at low alpha.
    set("alpha", JsValue::TRUE);
    set("premultipliedAlpha", JsValue::TRUE);
    // A laptop with two GPUs should use the fast one; this is a design tool.
    set("powerPreference", JsValue::from_str("high-performance"));
    opts.into()
}

/// Reads an integer GL parameter, or 0 if the driver declines to answer.
///
/// 0 is a legible failure here: every limit this asks for has a spec minimum
/// well above it, so a 0 in the report reads as "not answered" rather than as a
/// plausible-looking limit.
fn get_int(gl: &Gl, pname: u32) -> i32 {
    gl.get_parameter(pname)
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0) as i32
}

/// Whether `name` is available. `getExtension` returning `null` is the "no"; it
/// also returns `Err` on a lost context, which is likewise not a yes.
fn has_extension(gl: &Gl, name: &str) -> bool {
    matches!(gl.get_extension(name), Ok(Some(_)))
}

/// Installs the context-loss listeners.
///
/// `preventDefault` on `webglcontextlost` is not optional: without it the
/// browser never fires `webglcontextrestored` and the canvas is dead for the
/// life of the page.
fn watch_for_context_loss(canvas: &HtmlCanvasElement) -> Result<(), JsValue> {
    let lost = Closure::<dyn FnMut(web_sys::Event)>::new(|e: web_sys::Event| {
        e.prevent_default();
        CONTEXT_LOST.with(|c| c.set(true));
        web_sys::console::error_1(&JsValue::from_str(
            "hane webgl2: context lost -- every GL object is now dead; \
             waiting for webglcontextrestored",
        ));
    });
    let restored = Closure::<dyn FnMut(web_sys::Event)>::new(|_: web_sys::Event| {
        CONTEXT_LOST.with(|c| c.set(false));
        web_sys::console::log_1(&JsValue::from_str(
            "hane webgl2: context restored -- buffers, textures and programs \
             must be rebuilt",
        ));
    });

    canvas.add_event_listener_with_callback("webglcontextlost", lost.as_ref().unchecked_ref())?;
    canvas.add_event_listener_with_callback(
        "webglcontextrestored",
        restored.as_ref().unchecked_ref(),
    )?;

    // These live as long as the page does, so handing them to JS and dropping
    // the Rust side is the shape that matches the lifetime. The alternative --
    // storing them in a thread-local nothing ever removes from -- is the same
    // leak with more code around it.
    lost.forget();
    restored.forget();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything here is `web_sys` calls that need a browser, except the
    /// reporting -- which is the part a person reads and therefore the part
    /// worth pinning. Constructed by hand; `hane_gl_probe` itself cannot run
    /// outside a browser and is verified by loading the page.
    fn probe(missing: Vec<&'static str>) -> GlProbe {
        GlProbe {
            max_texture_size: 16384,
            max_draw_buffers: 8,
            float_render_targets: true,
            float_blend: true,
            float_linear_filter: true,
            missing,
        }
    }

    #[test]
    fn a_capable_machine_reports_ok() {
        let p = probe(Vec::new());
        assert!(p.supported());
        assert_eq!(p.missing_extensions(), "");
        assert_eq!(
            p.summary(),
            "hane webgl2: max texture 16384, max draw buffers 8, \
             float: render+blend+linear -- ok"
        );
    }

    /// The failure line has to name the extension, or it sends the reader to a
    /// search engine with "hane unsupported" and nothing else.
    #[test]
    fn a_missing_requirement_is_named_in_the_summary() {
        let p = probe(vec!["EXT_color_buffer_float"]);
        assert!(!p.supported());
        let s = p.summary();
        assert!(s.contains("UNSUPPORTED"), "{s}");
        assert!(s.contains("EXT_color_buffer_float"), "{s}");
    }

    #[test]
    fn several_missing_requirements_are_all_listed() {
        let p = probe(vec!["EXT_color_buffer_float", "EXT_float_blend"]);
        assert_eq!(
            p.missing_extensions(),
            "EXT_color_buffer_float, EXT_float_blend"
        );
    }

    /// Float support is three separate capabilities and the summary has to
    /// distinguish them: "render" and "render+blend" send #19 to different
    /// texture formats.
    #[test]
    fn partial_float_support_is_distinguished() {
        let mut p = probe(Vec::new());
        p.float_blend = false;
        p.float_linear_filter = false;
        assert!(p.summary().contains("float: render --"), "{}", p.summary());

        p.float_render_targets = false;
        assert!(p.summary().contains("float: none"), "{}", p.summary());
    }

    #[test]
    fn context_loss_starts_clear() {
        assert!(!hane_gl_context_lost());
    }
}
