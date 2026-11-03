#[path = "../../bridge/bridge_log.rs"]
mod bridge_log;
#[cfg(test)]
#[path = "../../bridge/bridge_core.rs"]
mod bridge_core;
mod doctor;
mod plan;
mod report;
mod restart;
#[path = "../../bridge/wire.rs"]
mod wire;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig},
    schemars, tool, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Tool input schemas are consumed by many clients and model providers. Keep them plain:
/// every property is an object with an explicit `type`, and optional fields are not `["x","null"]`.
fn flatten_schema(schema: &mut schemars::Schema) {
    let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    for (name, prop) in props.iter_mut() {
        let untyped = prop.is_boolean()
            || (prop.is_object()
                && prop.get("type").is_none()
                && prop.get("anyOf").is_none()
                && prop.get("$ref").is_none());
        if untyped {
            let mut plain = serde_json::Map::new();
            match name.as_str() {
                "sub_masks" => {
                    plain.insert("type".into(), json!("array"));
                    plain.insert("items".into(), json!({"type": "object"}));
                }
                "zoom" => {
                    plain.insert("type".into(), json!(["string", "number"]));
                }
                _ => {
                    plain.insert("type".into(), json!("object"));
                }
            }
            if let Some(description) = prop.get("description") {
                plain.insert("description".into(), description.clone());
            }
            *prop = Value::Object(plain);
        }
        if let Some(types) = prop.get("type").and_then(Value::as_array).cloned() {
            let kept: Vec<Value> = types.into_iter().filter(|t| t != "null").collect();
            prop["type"] = if kept.len() == 1 { kept[0].clone() } else { Value::Array(kept) };
        }
    }
}


#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Empty {}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Photo {
    /// Image path relative to the first allowed folder (forward slashes), or absolute inside an allowed folder.
    path: String,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct ListMasks {
    /// Image path relative to the first allowed folder (forward slashes), or absolute inside an allowed folder.
    path: String,
    /// "full" also returns every brush point; by default each stroke is summarised (tool, size, point count, bounding box).
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Folder {
    /// Folder relative to the work folder; "." is the work folder itself.
    folder: String,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Paths {
    /// Image paths relative to the work folder.
    paths: Vec<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct ListPhotos {
    /// "visible" (default, respects the active filters and sort) or "all".
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<u32>,
    /// Page size, at most 500 (default 100).
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<u32>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct SelectPhotos {
    /// Explicit photos to select. Takes precedence over scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    paths: Option<Vec<String>>,
    /// "visible" freezes the currently filtered list, "all" the whole folder, "none" clears.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Rating {
    paths: Vec<String>,
    /// Absolute rating from 0 (unrated) to 5. A rating above 0 clears a reject flag.
    stars: u8,
    /// "full" returns a row per photo even for big batches (default: batches over 20 return counts and failures only).
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Flag {
    paths: Vec<String>,
    /// "pick", "reject", or null to clear the flag.
    flag: Option<String>,
    /// "full" returns a row per photo even for big batches (default: batches over 20 return counts and failures only).
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Color {
    paths: Vec<String>,
    /// "red", "yellow", "green", "blue", "purple", or null to remove the label.
    color: Option<String>,
    /// "full" returns a row per photo even for big batches (default: batches over 20 return counts and failures only).
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Filter {
    /// Reset every filter before applying the other fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    clear: Option<bool>,
    /// Star threshold 0-5.
    #[serde(skip_serializing_if = "Option::is_none")]
    rating: Option<u8>,
    /// "gte" (at least), "eq" or "lte".
    #[serde(skip_serializing_if = "Option::is_none")]
    rating_operator: Option<String>,
    /// Color labels to show; empty list shows all.
    #[serde(skip_serializing_if = "Option::is_none")]
    colors: Option<Vec<String>>,
    /// "all", "rawOnly" or "nonRawOnly".
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_status: Option<String>,
    /// "all", "editedOnly" or "uneditedOnly".
    #[serde(skip_serializing_if = "Option::is_none")]
    edited_status: Option<String>,
    /// "all", "picked", "excludeRejected" or "rejected".
    #[serde(skip_serializing_if = "Option::is_none")]
    flag_status: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct View {
    /// "fit" or a number where 1 is 100%.
    #[serde(skip_serializing_if = "Option::is_none")]
    zoom: Option<Value>,
    /// "off", "toggle", "vertical", "horizontal" or "sideBySide".
    #[serde(skip_serializing_if = "Option::is_none")]
    compare: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct SetAdjustments {
    path: String,
    /// Partial adjustments; objects merge, arrays and scalars replace.
    patch: Value,
    /// Revision from get_adjustments; the call fails if the photo changed since.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_revision: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Preview {
    path: String,
    /// Longest side in pixels, 200-2400 (default 1200).
    #[serde(skip_serializing_if = "Option::is_none")]
    max_dimension: Option<u32>,
    /// "after" (default, current edit), "before" (original, keeping crop/rotation) or "side_by_side".
    #[serde(skip_serializing_if = "Option::is_none")]
    compare: Option<String>,
    /// Zoomed crop as fractions of the rendered image: {x, y, width, height}, each 0-1.
    /// With max_dimension at 2400 a small region approximates a 100% inspection.
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<Value>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct AddMask {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Components combined in order. Each: {type, mode?, invert?, opacity?, params?}.
    /// type: linear, radial, brush, all, ai-subject, ai-foreground, ai-sky, ai-text, ai-face,
    /// ai-dress (the wedding dress only: AI silhouette cleaned of skin, hair and other people).
    /// mode: additive (default), subtractive, intersect.
    /// Coordinates are fractions (0-1) of the oriented, uncropped image.
    /// linear params: startX, startY, endX, endY, range. radial params: centerX, centerY,
    /// radiusX, radiusY (fractions of width), rotation (degrees), feather 0-1.
    /// brush params: lines [{tool: brush|eraser, size (fraction of long edge), feather 0-1,
    /// points [{x, y}]}]. ai-subject params: box {x1, y1, x2, y2}. ai-text params: prompt.
    /// ai-face params: parts among faceSkin, bodySkin, hair, eyebrows, eyeWhites, iris, etc.
    /// ai-dress params: prompt (default the bride's white wedding dress), region {x1,y1,x2,y2} to
    /// keep only that box, exclude [{x1,y1,x2,y2}] boxes to drop (a bridesmaid, the groom),
    /// chroma_scale (above 1 keeps more colour variation). ai-* also accept grow and feather.
    /// A brush component with mode subtractive is how to cut a face out of an ai-text mask:
    /// brush edits on an AI component are ignored.
    sub_masks: Value,
    /// Remove any existing mask with this name in the same step (keeps one mask per photo).
    #[serde(skip_serializing_if = "Option::is_none")]
    replace_name: Option<String>,
    /// Remove this mask id in the same step.
    #[serde(skip_serializing_if = "Option::is_none")]
    replace_mask_id: Option<String>,
    /// Local adjustments applied through the mask (same names as global ones, e.g. temperature,
    /// tint, saturation, exposure, highlights, hsl). Defaults leave the pixels untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    adjustments: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    invert: Option<bool>,
    /// Mask strength 0-100.
    #[serde(skip_serializing_if = "Option::is_none")]
    opacity: Option<f64>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct SelectDress {
    /// Photos to process, one after another (max 60). Each is opened, masked and checked.
    paths: Vec<String>,
    /// Mask name; an existing mask with the same name is replaced, so reruns never duplicate it.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Local adjustments for the mask. Default removes the blue cast: hsl blues -100, aquas -60, purples -40.
    #[serde(skip_serializing_if = "Option::is_none")]
    adjustments: Option<Value>,
    /// Segmentation prompt. Default "white wedding dress of the bride"; fallbacks are tried when it finds nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    /// Keep only this box {x1,y1,x2,y2} (fractions of the oriented, uncropped image) for every photo.
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<Value>,
    /// Boxes [{x1,y1,x2,y2}] to drop for every photo (a bridesmaid, the groom's shirt).
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude: Option<Value>,
    /// Above 1 keeps more colour variation inside the dress; below 1 cuts harder.
    #[serde(skip_serializing_if = "Option::is_none")]
    chroma_scale: Option<f64>,
    /// Also return one contact sheet of the masks (red overlay) for up to 24 photos.
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<bool>,
    /// "neutral_white": after the mask is made, nudge its temperature and tint until the inside of the dress measures neutral (no yellow, green or blue cast). The result row reports the values used.
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct SelectSkin {
    /// Photos to process, one after another (max 60). Each is opened, masked and measured.
    paths: Vec<String>,
    /// Mask name; an existing mask with the same name is replaced only when the new one is made, so reruns never lose it.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Starting local adjustments (default exposure 0.3, shadows 12, highlights -10, clarity -8). Exposure, saturation and temperature are then tuned from the measured skin.
    #[serde(skip_serializing_if = "Option::is_none")]
    adjustments: Option<Value>,
    /// Skin luminance to reach, 0-255 (default 135).
    #[serde(skip_serializing_if = "Option::is_none")]
    target_luminance: Option<f64>,
    /// AI face parts to select (default faceSkin and bodySkin).
    #[serde(skip_serializing_if = "Option::is_none")]
    parts: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Reference {
    /// Image whose look is to be copied: a file inside an allowed folder (a chat attachment is not a file the
    /// editor can read; copy it into the work folder first). A finished JPEG works best.
    reference: String,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct CompareToReference {
    /// The photo to compare, from the open folder.
    path: String,
    /// The reference image, inside an allowed folder.
    reference: String,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct StylePhoto {
    /// The photo to restyle, from the open folder.
    path: String,
    /// The reference image, inside an allowed folder.
    reference: String,
    /// Adjustments to apply, merged like set_adjustments: exposure, contrast, highlights, shadows, whites, blacks,
    /// temperature, tint, saturation, vibrance, hsl {reds..magentas: {hue, saturation, luminance}},
    /// colorGrading {shadows|midtones|highlights|global: {hue, saturation, luminance}, balance, blending},
    /// curves {luma|red|green|blue: [{x, y}]} (x and y 0-255; raise the first luma point's y for faded blacks).
    #[serde(skip_serializing_if = "Option::is_none")]
    patch: Option<Value>,
    /// First clear the look (tone, colour, HSL, grading, curves) back to neutral, so the patch is the whole look of this photo. Masks, crop and detail settings stay. Default false.
    #[serde(skip_serializing_if = "Option::is_none")]
    reset: Option<bool>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct ReviewSet {
    /// The reference image, inside an allowed folder.
    reference: String,
    /// The photos to review (max 24), from the open folder.
    paths: Vec<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct MatchStyle {
    /// Image whose look is to be copied, inside an allowed folder (see analyze_reference).
    reference: String,
    /// Photos to restyle, one after another (max 60), all from the open folder. Give the whole set to get a consistent gallery.
    paths: Vec<String>,
    /// How far each photo moves toward the reference, 0-1 (default 0.8). Every photo keeps its own light: a dark room is not turned into daylight.
    #[serde(skip_serializing_if = "Option::is_none")]
    strength: Option<f64>,
    /// How much the photos are pulled toward each other, 0-1 (default 0.5). 0 respects each photo's own light, 1 gives all the same look.
    #[serde(skip_serializing_if = "Option::is_none")]
    coherence: Option<f64>,
    /// What to copy, any of: tone (brightness, contrast, highlights, blacks), color (warmth and tint), grading (shadow and highlight colours), saturation. Default all four.
    #[serde(skip_serializing_if = "Option::is_none")]
    what: Option<Vec<String>>,
    /// Measure and report what would change without touching any photo (default false).
    #[serde(skip_serializing_if = "Option::is_none")]
    dry_run: Option<bool>,
    /// Also return one contact sheet with the reference first and the result of each photo, for up to 24 photos.
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<bool>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct UpdateMask {
    path: String,
    mask_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    adjustments: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    invert: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    opacity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visible: Option<bool>,
    /// Component edits: [{id, mode?, invert?, opacity?, visible?, params?, add_lines?, clear_lines?}].
    /// add_lines appends brush/eraser strokes to refine a selection.
    #[serde(skip_serializing_if = "Option::is_none")]
    sub_masks: Option<Value>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct MaskRef {
    path: String,
    mask_id: String,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct MaskOverlay {
    path: String,
    mask_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_dimension: Option<u32>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Export {
    paths: Vec<String>,
    /// Destination folder inside the work folder (default "exports"). Must not hold the originals.
    #[serde(skip_serializing_if = "Option::is_none")]
    subfolder: Option<String>,
    /// "jpeg" (default), "png", "tiff" or "webp".
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<String>,
    /// JPEG quality 1-100 (default 92).
    #[serde(skip_serializing_if = "Option::is_none")]
    quality: Option<u8>,
    /// Resize so the long edge is at most this many pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_dimension: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filename_template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_metadata: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    strip_gps: Option<bool>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct OptFolder {
    /// Folder to inspect (relative to the first allowed folder, or absolute inside an allowed one). Omit to list the allowed root folders.
    #[serde(skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Analyze {
    path: String,
    /// "after" (default, current edit) or "before" (original, keeping crop/rotation). Ignored with mask_id.
    #[serde(skip_serializing_if = "Option::is_none")]
    compare: Option<String>,
    /// Restrict to a region: fractions {x, y, width, height}, each 0-1.
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<Value>,
    /// Also report coverage, bounding box and colors inside vs outside this mask (active photo only).
    #[serde(skip_serializing_if = "Option::is_none")]
    mask_id: Option<String>,
    /// Analysis resolution, longest side 200-1200 (default 600).
    #[serde(skip_serializing_if = "Option::is_none")]
    max_dimension: Option<u32>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Transform {
    path: String,
    /// Quarter turns clockwise, 0-3 (absolute, not cumulative).
    #[serde(skip_serializing_if = "Option::is_none")]
    rotate_steps: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flip_horizontal: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flip_vertical: Option<bool>,
    /// Horizon straightening in degrees, -45..45 (the crop follows automatically).
    #[serde(skip_serializing_if = "Option::is_none")]
    straighten_degrees: Option<f64>,
    /// Crop as fractions {x, y, width, height} of the oriented image.
    #[serde(skip_serializing_if = "Option::is_none")]
    crop: Option<Value>,
    /// Centered crop with this width/height ratio (e.g. 1.5 for 3:2, 0.8 for 4:5).
    #[serde(skip_serializing_if = "Option::is_none")]
    aspect_ratio: Option<f64>,
    /// Remove any crop and aspect ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    clear_crop: Option<bool>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct AutoAdjust {
    path: String,
    /// "subtle", "normal" (default) or "strong".
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Tags {
    paths: Vec<String>,
    /// Tags to add (free text, for example "ceremony").
    #[serde(skip_serializing_if = "Option::is_none")]
    add: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remove: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct ApplyPreset {
    path: String,
    /// Preset id or exact name from list_presets.
    preset: String,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct JobRef {
    /// Job id from a reply that said the work is still running or queued.
    job: String,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct JobWait {
    /// Job id from a reply that said the work is still running or queued.
    job: String,
    /// Hold the answer up to this many seconds (at most 40) until the job finishes.
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_s: Option<u32>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Assess {
    /// Photos to measure, at most 400 per call; one file per shot (use the RAW of a RAW+JPEG pair).
    paths: Vec<String>,
    /// Look for faces and measure the main face (default true). False is faster and ranks on the sharpest region.
    #[serde(skip_serializing_if = "Option::is_none")]
    faces: Option<bool>,
    /// Also write the suggested star ratings to the photos (default false).
    #[serde(skip_serializing_if = "Option::is_none")]
    apply_stars: Option<bool>,
    /// "summary" (default) or "full" to include one row of numbers per photo.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct PlanTask {
    /// What the user wants, in their own words.
    goal: String,
    /// Folder to plan for (relative to the first allowed folder). Default: the work folder itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = flatten_schema)]
struct Restart {
    /// Restart even if the editor still answers (default false: a healthy editor is left alone).
    #[serde(skip_serializing_if = "Option::is_none")]
    force: Option<bool>,
}

#[derive(Clone, Default)]
struct Editor {
    /// Set once plan_task has run in this session; bulk and destructive tools mention it when it has not.
    planned: Arc<AtomicBool>,
    /// When the editor was last restarted by the server itself, so it is not restarted in a loop.
    recovered: Arc<std::sync::Mutex<Option<Instant>>>,
    /// The Markdown usage report of this session (%APPDATA%\RapidRAW-MCP\reports).
    report: Arc<report::Report>,
}

/// What happened when we tried to bring the editor up for an agent.
#[cfg(windows)]
enum Launch {
    Started,
    /// Somebody else (another agent) is starting it right now.
    AlreadyRunning,
    /// The editor is open already, usually the user's own window: never touched, only waited for.
    Open,
    /// The stock RapidRAW is open: it has no bridge, and a second copy would fight over the same settings.
    StockOpen,
    NotInstalled,
}

/// Start the MCP RapidRAW installed next to this server, unless one is already up or starting.
#[cfg(windows)]
fn launch_editor() -> Launch {
    use std::os::windows::process::CommandExt;
    let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) else {
        return Launch::NotInstalled;
    };
    let app = dir.join("MCP RapidRAW.exe");
    if !app.is_file() {
        return Launch::NotInstalled;
    }
    let listing = std::process::Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase())
        .unwrap_or_default();
    if listing.contains("\"mcp rapidraw.exe\"") {
        return Launch::Open;
    }
    if listing.contains("\"rapidraw.exe\"") {
        return Launch::StockOpen;
    }
    // Several agents may ask at once: a fresh lock file means someone is already starting it.
    let lock = std::env::temp_dir().join("rapidraw-mcp-launch.lock");
    let fresh = std::fs::metadata(&lock)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(60));
    if fresh {
        return Launch::AlreadyRunning;
    }
    let _ = std::fs::write(&lock, b"");
    let spawned = std::process::Command::new(&app)
        .current_dir(&dir)
        // The editor must not share our stdio: stdout is the MCP protocol channel.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0000_0008 | 0x0000_0200) // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        .spawn();
    if spawned.is_ok() { Launch::Started } else { Launch::NotInstalled }
}

#[cfg(windows)]
fn pipe_missing() -> bool {
    match wire::key() {
        Ok(key) => tokio::net::windows::named_pipe::ClientOptions::new()
            .open(wire::pipe_name(&key))
            .err()
            .is_some_and(|e| e.raw_os_error() == Some(2)),
        Err(_) => true,
    }
}
#[cfg(not(windows))]
fn pipe_missing() -> bool {
    true
}

#[cfg(windows)]
async fn request(cmd: &Value) -> std::io::Result<Value> {
    let key = wire::key()?;
    let name = wire::pipe_name(&key);
    let started = std::time::Instant::now();
    let mut launched = false;
    // An editor that was opened by hand and has no bridge will not get one by waiting: stop sooner.
    let mut patience = 90;
    let mut pipe = loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(&name) {
            Ok(p) => break p,
            // 231: every instance is serving a client; wait for the next one to listen.
            Err(e) if e.raw_os_error() == Some(231) && started.elapsed() < Duration::from_secs(10) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            // 2: no editor yet. Open it for the agent, then wait for its bridge to come up.
            Err(e) if e.raw_os_error() == Some(2) && (launched || started.elapsed() < Duration::from_secs(1)) => {
                if !launched {
                    let outcome = launch_editor();
                    bridge_log::server(match outcome {
                        Launch::Started => "pipe missing: started MCP RapidRAW",
                        Launch::AlreadyRunning => "pipe missing: editor being started by another agent",
                        Launch::Open => "pipe missing: the editor is open (not started by this server) but its bridge is not listening yet",
                        Launch::StockOpen => "pipe missing: the regular RapidRAW is open (no bridge)",
                        Launch::NotInstalled => "pipe missing: MCP RapidRAW is not installed next to the server",
                    });
                    match outcome {
                        Launch::Started | Launch::AlreadyRunning => launched = true,
                        Launch::Open => {
                            launched = true;
                            patience = 30;
                        }
                        Launch::StockOpen => {
                            return Err(std::io::Error::other(
                                "[stock_rapidraw_open] The regular RapidRAW is open and has no agent bridge. Close it and retry: MCP RapidRAW will open by itself.",
                            ));
                        }
                        Launch::NotInstalled => {
                            return Err(std::io::Error::other(format!(
                                "[editor_not_installed] Open MCP RapidRAW by hand, or install it with scripts/install.ps1: {e}"
                            )));
                        }
                    }
                }
                if started.elapsed() > Duration::from_secs(patience) {
                    bridge_log::server(&format!("gave up: the editor bridge did not come up in {patience} s"));
                    return Err(std::io::Error::other(if patience < 90 {
                        "[editor_without_bridge] MCP RapidRAW is open but its agent bridge is not listening (it was opened before the bridge could start, or it is an older build). It was not closed or restarted so the user's session is not interrupted: tell the user, and ask them to close and reopen MCP RapidRAW when it suits them. The bridge starts by itself on opening; bridge.log says why it did not."
                    } else {
                        "[bridge_not_started] MCP RapidRAW was started but its bridge did not come up in 90 s; check that a folder is configured (scripts/setup.ps1) and read %APPDATA%\\RapidRAW-MCP\\bridge.log."
                    }));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                bridge_log::server(&format!(
                    "pipe open failed after {} ms (os error {:?}): {e}",
                    started.elapsed().as_millis(),
                    e.raw_os_error()
                ));
                return Err(std::io::Error::other(format!(
                    "[editor_not_open] Check that the MCP-enabled RapidRAW is running: {e}"
                )));
            }
        }
    };
    let connected = started.elapsed().as_millis();
    if connected > 1000 {
        bridge_log::server(&format!("pipe connected after {connected} ms (waited for a free instance or the editor)"));
    }
    wire::authenticate(&mut pipe, &key, false).await?;
    wire::write_frame(&mut pipe, &serde_json::to_vec(cmd)?, wire::MAX_COMMAND).await?;
    let reply = wire::read_frame(&mut pipe, wire::MAX_REPLY).await?;
    // Let the editor drain its write before the pipe is torn down.
    let _ = tokio::io::AsyncWriteExt::write_all(&mut pipe, &[0]).await;
    Ok(serde_json::from_slice(&reply)?)
}
#[cfg(not(windows))]
async fn request(_: &Value) -> std::io::Result<Value> {
    Err(std::io::Error::other("Windows bridge required"))
}

impl Editor {
    /// Close the editor and open it again. Everything is saved as you go; running jobs are lost.
    async fn restart(&self) -> Result<Value, String> {
        let killed = restart::kill_editor().map_err(|e| format!("[restart_failed] {e}"))?;
        bridge_log::server(&format!("restart_editor: editor closed={killed}"));
        restart::clear_launch_lock();
        let gpu_reset = restart::reset_gpu_backend();
        // Give Windows a moment to drop the pipe, then let the normal launcher bring the editor back.
        restart::wait_until(Duration::from_secs(15), || pipe_missing()).await;
        match self.exchange_once("status", json!({})).await {
            Ok(status) => Ok(json!({
                "restarted": true,
                "closed_old_editor": killed,
                "gpu_settings_reset": gpu_reset,
                "status": status,
                "note": "Photos and edits were saved as you went. Jobs that were running in the old editor are gone: check with list_photos or get_photo what finished and repeat the rest."
            })),
            Err(m) => Err(format!("[restart_failed] The old editor was closed but the new one did not come up: {m}")),
        }
    }

    /// A request to the editor. If the editor lost its GPU and stopped answering it is restarted once (at most
    /// every ten minutes) instead of leaving every agent to time out; the call itself is not repeated, since
    /// whatever it was doing may have half happened.
    async fn exchange(&self, op: &str, params: impl Serialize) -> Result<Value, String> {
        let params = serde_json::to_value(params).map_err(|_| "Invalid parameters".to_string())?;
        let first = self.exchange_once(op, params).await;
        let Err(message) = &first else {
            return first;
        };
        if !message.contains("[ui_not_responding]") || !restart::gpu_trouble_logged() {
            return first;
        }
        {
            let mut last = self.recovered.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
                return first;
            }
            *last = Some(Instant::now());
        }
        // Make sure it is not merely busy with a heavy step.
        tokio::time::sleep(Duration::from_secs(3)).await;
        match self.exchange_once("status", json!({})).await {
            Err(again) if again.contains("[ui_not_responding]") => {
                bridge_log::server("the editor lost its GPU and no longer answers: restarting it");
                self.report.note("the editor lost its GPU and stopped answering: the server restarted it by itself");
                match self.restart().await {
                    Ok(_) => Err("[editor_restarted] The editor had lost its GPU and stopped answering, so it was restarted. Photos and edits are saved; jobs that were running are gone. Check what finished (list_photos or get_photo) and repeat the rest.".into()),
                    Err(e) => Err(format!("{message} (the automatic restart failed: {e})")),
                }
            }
            _ => first,
        }
    }

    /// One request to the editor. Err carries a message ready to show the agent.
    async fn exchange_once(&self, op: &str, params: Value) -> Result<Value, String> {
        let mut cmd = match params {
            Value::Object(map) => Value::Object(map),
            _ => return Err("Invalid parameters".into()),
        };
        cmd["op"] = json!(op);
        cmd["proto"] = json!(wire::PROTOCOL);
        // Short id that also travels to the editor, so bridge.log can follow one call across processes.
        let call = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        cmd["call"] = json!(call);
        let began = Instant::now();
        bridge_log::server(&format!("{call} {op} start"));
        // Queuing is the editor's job: it runs one job at a time, in order, and answers a call that is still
        // going after ~45 s with the job's id instead of making the agent wait for a client timeout.
        // This budget only covers a stuck pipe or an editor that never comes up (up to 90 s to start).
        let budget = Duration::from_secs(300);
        let outcome = tokio::time::timeout(budget, request(&cmd)).await;
        let summary = match &outcome {
            Ok(Ok(v)) if v["error"].is_string() => format!("error: {}", v["error"].as_str().unwrap_or("")),
            Ok(Ok(_)) => "ok".to_string(),
            Ok(Err(e)) => format!("transport error: {e}"),
            Err(_) => format!("server budget of {} s elapsed", budget.as_secs()),
        };
        bridge_log::server(&format!("{call} {op} done in {} ms: {summary}", began.elapsed().as_millis()));
        let result = match outcome {
            Ok(Ok(v)) if v["error"].is_string() => Err(v["error"].as_str().unwrap().to_string()),
            Ok(Ok(v)) => Ok(v),
            // Errors that already carry a [code] come from the launcher above; anything else is the pipe breaking.
            Ok(Err(e)) if e.to_string().starts_with('[') => Err(e.to_string()),
            Ok(Err(e)) => Err(format!(
                "[bridge_connection] Lost the connection to the editor: {e}. The operation may have completed; check status before retrying."
            )),
            Err(_) => Err("[server_timeout] Timed out waiting for the editor; the operation may have completed, check status before retrying.".into()),
        };
        self.report.call(op, &call, began.elapsed(), result.as_ref().map_err(String::as_str));
        result
    }

    async fn run(&self, op: &str, params: impl Serialize) -> CallToolResult {
        match self.exchange(op, params).await {
            // An editor opened before this operation existed does not know it; the protocol number did not change
            // on purpose, so everything else keeps working with that editor.
            Err(message) if message == "Unknown operation" && matches!(op, "look" | "match_style" | "style_photo" | "review_set") => CallToolResult::error(vec![ContentBlock::text(
                "[editor_outdated] The open editor was started before this tool existed. Everything else works; to use this one, ask the user to close and reopen MCP RapidRAW when it suits them (do not restart it yourself).",
            )]),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
            Ok(mut v) => {
                // Work that is hard to undo or takes long should follow a plan; say so once instead of refusing.
                if matches!(op, "trash" | "select_dress" | "export" | "assess" | "match_style")
                    && v.get("job").is_none()
                    && !self.planned.load(Ordering::Relaxed)
                    && let Some(map) = v.as_object_mut()
                {
                    map.insert("note".into(), json!("No plan was made in this session. For a request with several steps, call plan_task first and follow it."));
                }
                let mut content = Vec::new();
                if let Some(image) = v
                    .as_object_mut()
                    .and_then(|o| o.remove("jpeg"))
                    .and_then(|v| v.as_str().map(str::to_owned))
                {
                    content.push(ContentBlock::image(image, "image/jpeg"));
                }
                content.push(ContentBlock::text(v.to_string()));
                CallToolResult::success(content)
            }
        }
    }
}

#[tool_router]
impl Editor {
    #[tool(description = "Connection check and live editor state: open folder, visible/total photos, selection, active photo, filters, undo/redo availability. Reflects manual changes made in the UI.")]
    async fn status(&self, Parameters(p): Parameters<Empty>) -> CallToolResult {
        self.run("status", p).await
    }
    #[tool(description = "Open a folder of the work folder in the RapidRAW library (use \".\" for the work folder itself).")]
    async fn open_folder(&self, Parameters(p): Parameters<Folder>) -> CallToolResult {
        self.run("open_folder", p).await
    }
    #[tool(description = "List photos with rating, flag, color label, edited and raw state. Paged; scope visible follows the active filters.")]
    async fn list_photos(&self, Parameters(p): Parameters<ListPhotos>) -> CallToolResult {
        self.run("list_photos", p).await
    }
    #[tool(description = "Set the library selection to explicit photos, or freeze the visible/all/none set. Returns exactly what was selected.")]
    async fn select_photos(&self, Parameters(p): Parameters<SelectPhotos>) -> CallToolResult {
        self.run("select_photos", p).await
    }
    #[tool(description = "Open a photo in the editor and wait until it is loaded. Needed before editing, masks or undo on that photo.")]
    async fn open_photo(&self, Parameters(p): Parameters<Photo>) -> CallToolResult {
        self.run("open_photo", p).await
    }
    #[tool(description = "Read one photo's persisted rating, flag and color; if it is the active photo, also size, revision, history position and mask count.")]
    async fn get_photo(&self, Parameters(p): Parameters<Photo>) -> CallToolResult {
        self.run("get_photo", p).await
    }
    #[tool(description = "Set absolute star ratings (0-5) on photos. Returns per-photo results verified from the saved metadata.")]
    async fn set_rating(&self, Parameters(p): Parameters<Rating>) -> CallToolResult {
        self.run("set_rating", p).await
    }
    #[tool(description = "Mark photos as pick, reject, or clear the mark. Returns per-photo results verified from the saved metadata.")]
    async fn set_flag(&self, Parameters(p): Parameters<Flag>) -> CallToolResult {
        self.run("set_flag", p).await
    }
    #[tool(description = "Set or clear the color label of photos. Returns per-photo results verified from the saved metadata.")]
    async fn set_color(&self, Parameters(p): Parameters<Color>) -> CallToolResult {
        self.run("set_color", p).await
    }
    #[tool(description = "Change the library filters (stars with operator, color labels, raw/edited/flag status). Returns the filters and how many photos remain visible.")]
    async fn set_filter(&self, Parameters(p): Parameters<Filter>) -> CallToolResult {
        self.run("set_filter", p).await
    }
    #[tool(description = "Send explicitly listed photos (max 200) to the Recycle Bin together with their sidecars. Returns per-file results checked on disk. Never accepts a scope; list the exact files.")]
    async fn trash_photos(&self, Parameters(p): Parameters<Paths>) -> CallToolResult {
        self.run("trash", p).await
    }
    #[tool(description = "Copy the adjustments of a photo using RapidRAW's copy settings.")]
    async fn copy_adjustments(&self, Parameters(p): Parameters<Photo>) -> CallToolResult {
        self.run("copy_adjustments", p).await
    }
    #[tool(description = "Paste the copied adjustments onto photos using RapidRAW's paste settings (merge or replace).")]
    async fn paste_adjustments(&self, Parameters(p): Parameters<Paths>) -> CallToolResult {
        self.run("paste_adjustments", p).await
    }
    #[tool(description = "Change editor zoom (\"fit\" or 1 = 100%) and the compare mode.")]
    async fn set_view(&self, Parameters(p): Parameters<View>) -> CallToolResult {
        self.run("set_view", p).await
    }
    #[tool(description = "Read all global adjustments of the active photo with their current values, the list of keys that differ from default, a revision id, and mask counts. Typical ranges: exposure about -5..5 stops; brightness, contrast, highlights, shadows, whites, blacks, saturation, vibrance, clarity, structure, dehaze, temperature, tint -100..100; sharpness, luma/color noise reduction 0..100.")]
    async fn get_adjustments(&self, Parameters(p): Parameters<Photo>) -> CallToolResult {
        self.run("get_adjustments", p).await
    }
    #[tool(description = "Apply a partial change to the active photo's global adjustments through the editor (history, autosave and live render included). Validates keys and types, rejects stale revisions, and confirms the result was saved. Masks are edited with the mask tools.")]
    async fn set_adjustments(&self, Parameters(p): Parameters<SetAdjustments>) -> CallToolResult {
        self.run("set_adjustments", p).await
    }
    #[tool(description = "Reset the adjustments of the listed photos to defaults.")]
    async fn reset_adjustments(&self, Parameters(p): Parameters<Paths>) -> CallToolResult {
        self.run("reset_adjustments", p).await
    }
    #[tool(description = "Undo the last edit step of the active photo and save.")]
    async fn undo(&self, Parameters(p): Parameters<Photo>) -> CallToolResult {
        self.run("undo", p).await
    }
    #[tool(description = "Redo the last undone edit step of the active photo and save.")]
    async fn redo(&self, Parameters(p): Parameters<Photo>) -> CallToolResult {
        self.run("redo", p).await
    }
    #[tool(description = "Render a JPEG you can see: current edit, original, or side by side, optionally a zoomed region. Reports revision, output size and native size of the region.")]
    async fn preview_photo(&self, Parameters(p): Parameters<Preview>) -> CallToolResult {
        self.run("preview", p).await
    }
    #[tool(description = "List the local masks of the active photo with components, parameters (AI bitmaps omitted) and non-default adjustments. Brush strokes are summarised (tool, size, point count, bounding box in image pixels); pass detail=full for every point.")]
    async fn list_masks(&self, Parameters(p): Parameters<ListMasks>) -> CallToolResult {
        self.run("list_masks", p).await
    }
    #[tool(description = "Create a local mask (geometric, brush or AI segmentation) with optional local adjustments on the active photo. AI components run RapidRAW's segmentation and report when no mask was produced. Always inspect the result with mask_overlay. For a wedding dress use the ai-dress component (or select_dress for many photos): it cleans the AI silhouette so only the dress stays.")]
    async fn add_mask(&self, Parameters(p): Parameters<AddMask>) -> CallToolResult {
        self.run("add_mask", p).await
    }
    #[tool(description = "Select the bride's wedding dress in many photos in one call and apply local adjustments only there (default: remove the blue cast). Per photo it opens the image, asks the AI for the dress and cleans the result with the photo's own pixels, so the bride's face, hair and skin, bridesmaids, the groom and stray bits are dropped and only the dress stays. An existing mask with the same name is replaced, so reruns never duplicate it. Returns one compact row per photo (coverage, bbox, how much was removed, warnings) and, with preview, a contact sheet. Photos without a bride come back not ok; leave them alone. If a row looks wrong, rerun that photo with region or exclude boxes.")]
    async fn select_dress(&self, Parameters(p): Parameters<SelectDress>) -> CallToolResult {
        self.run("select_dress", p).await
    }
    #[tool(description = "Lighten the skin of many photos in one call: per photo it opens the image, selects face and body skin with the AI face mask and then measures the skin inside the mask, adjusting exposure, saturation and warmth in up to 3 steps until the skin reaches the target luminance without going pale or grey. A photo whose mask cannot be made comes back not ok and keeps its previous skin mask. Rows report the measured luminance, saturation and warmth, and warn when the skin is still dark, pale or grey. The lift is capped at +0.7 EV so dark skin is lightened, not bleached.")]
    async fn select_skin(&self, Parameters(p): Parameters<SelectSkin>) -> CallToolResult {
        self.run("select_skin", p).await
    }
    #[tool(description = "Read a reference image the way a photographer does, and see it: how its WHITES are made (level and colour cast, measured on near-neutral pixels so the cast of the light and of the grade shows, not the colour of objects), how its BLACKS are made (lifted and faded, or deep; their tint), and how each colour family behaves (reds, oranges, yellows, greens, aquas, blues, purples, magentas: share of the picture, saturation, luminance, average hue), plus overall tone, as numbers and in words, and the picture itself as the editor renders it. LOOK AT THE PICTURE and think how it was made before reading the numbers, then write the recipe in photographic terms and tell the user. Then work photo by photo with compare_to_reference and style_photo. The reference must be a file inside an allowed folder: an image attached to the chat is not a file the editor can read, so copy it into the work folder first.")]
    async fn analyze_reference(&self, Parameters(p): Parameters<Reference>) -> CallToolResult {
        self.run("look", p).await
    }
    #[tool(description = "Put a photo next to the reference and read what differs, as a photographer would. Returns ONE picture (reference, then the photo as it is now) and the measured palette of both: whites (level, cast), blacks (level, cast) and each colour family (share, saturation, luminance, hue), plus to_get_closer: measured differences turned into suggestions in the editor's own controls (for example: greens are 62% saturated against the reference's 34%, lower greens saturation about -40). The suggestions are not orders: look at the picture and decide what suits THIS photo's light and subject. Changes nothing.")]
    async fn compare_to_reference(&self, Parameters(p): Parameters<CompareToReference>) -> CallToolResult {
        self.run("style_photo", p).await
    }
    #[tool(description = "Restyle ONE photo toward the reference and see the result at once: opens the photo, optionally clears its look (reset), applies your patch (global adjustments, hsl per colour family, colorGrading, curves), saves it, and returns ONE picture (reference, the photo before, the photo after) with the measured palette before and after and what still differs. This is the main tool: decide each photo's settings yourself from what you see (its light, its whites, its skin, its colours), apply, look at the picture, and correct once or twice until it feels like the reference without looking like a filter. Different photos get different settings. Do not paste one patch to all.")]
    async fn style_photo(&self, Parameters(p): Parameters<StylePhoto>) -> CallToolResult {
        self.run("style_photo", p).await
    }
    #[tool(description = "One sheet with the reference first and then up to 24 photos as they are now, numbered, plus per-photo whites, blacks, brightness and green saturation and the spread of brightness, warmth and saturation across the set. Use it after styling to judge the set as a gallery: find the photos that break it and fix them with style_photo. Changes nothing.")]
    async fn review_set(&self, Parameters(p): Parameters<ReviewSet>) -> CallToolResult {
        self.run("review_set", p).await
    }
    #[tool(description = "EXPERIMENTAL mechanical pass, not a faithful style: moves the TONE of many photos (brightness, contrast, highlights, blacks) toward a reference in a closed loop and pulls the set toward each other. It does not understand whites, colours or the light of each scene, and colour taken from averages follows the subject (a green field makes every photo green), so colour families are off unless asked for with what. For a faithful style do not use it as the main tool: use analyze_reference, compare_to_reference and style_photo photo by photo. Use dry_run first; a long job, follow it with job_status.")]
    async fn match_style(&self, Parameters(p): Parameters<MatchStyle>) -> CallToolResult {
        self.run("match_style", p).await
    }
    #[tool(description = "Edit a mask: its local adjustments, name, strength, invert, visibility, or its components (change mode, add brush/eraser strokes to refine, adjust geometry).")]
    async fn update_mask(&self, Parameters(p): Parameters<UpdateMask>) -> CallToolResult {
        self.run("update_mask", p).await
    }
    #[tool(description = "Delete a mask from the active photo.")]
    async fn delete_mask(&self, Parameters(p): Parameters<MaskRef>) -> CallToolResult {
        self.run("delete_mask", p).await
    }
    #[tool(description = "Return the photo with the mask coverage overlaid in red so selection area and edges can be inspected.")]
    async fn mask_overlay(&self, Parameters(p): Parameters<MaskOverlay>) -> CallToolResult {
        self.run("mask_overlay", p).await
    }
    #[tool(description = "Export photos with their saved edits into a subfolder of the work folder using RapidRAW's export engine. Returns the files actually written (checked on disk). Originals are never overwritten.")]
    async fn export_photos(&self, Parameters(p): Parameters<Export>) -> CallToolResult {
        self.run("export", p).await
    }
    #[tool(description = "Discover folders without touching the UI: with no argument lists the allowed root folders; with a folder lists its subfolders and image counts.")]
    async fn list_folders(&self, Parameters(p): Parameters<OptFolder>) -> CallToolResult {
        self.run("list_folders", p).await
    }
    #[tool(description = "Numbers instead of pictures, for models that cannot see images: mean RGB, luminance percentiles, clipped shadows/highlights, saturation, warm/cool balance and a 3x3 luminance map of the whole photo or a region. With mask_id also the mask coverage, bounding box, and the same stats inside vs outside the mask (for example to check a color cast on a dress).")]
    async fn analyze_photo(&self, Parameters(p): Parameters<Analyze>) -> CallToolResult {
        self.run("analyze", p).await
    }
    #[tool(description = "Rotate in quarter turns, flip, straighten the horizon, and crop (free fractions, aspect ratio, or clear) on the active photo. Returns the resulting crop in pixels.")]
    async fn transform_photo(&self, Parameters(p): Parameters<Transform>) -> CallToolResult {
        self.run("transform", p).await
    }
    #[tool(description = "Run the automatic adjustment of RapidRAW on the active photo (subtle, normal or strong). Returns which values changed.")]
    async fn auto_adjust(&self, Parameters(p): Parameters<AutoAdjust>) -> CallToolResult {
        self.run("auto_adjust", p).await
    }
    #[tool(description = "Add or remove free-text tags on photos. Returns per-photo results verified from the saved metadata.")]
    async fn set_tags(&self, Parameters(p): Parameters<Tags>) -> CallToolResult {
        self.run("set_tags", p).await
    }
    #[tool(description = "List the presets saved in RapidRAW (id, name, folder, which settings they change).")]
    async fn list_presets(&self, Parameters(p): Parameters<Empty>) -> CallToolResult {
        self.run("list_presets", p).await
    }
    #[tool(description = "Apply a preset by id or name to the active photo at full strength; presets with AI masks are skipped with a warning.")]
    async fn apply_preset(&self, Parameters(p): Parameters<ApplyPreset>) -> CallToolResult {
        self.run("apply_preset", p).await
    }
    #[tool(description = "Reference of every global adjustment: default, type and slider range, plus structured groups (hsl, colorGrading, ...) and the settings usable inside masks. Read it before building a set_adjustments patch.")]
    async fn describe_adjustments(&self, Parameters(p): Parameters<Empty>) -> CallToolResult {
        self.run("describe_adjustments", p).await
    }
    #[tool(description = "Cancel a running export.")]
    async fn cancel_export(&self, Parameters(p): Parameters<Empty>) -> CallToolResult {
        self.run("cancel_export", p).await
    }
    #[tool(description = "Follow a long operation by its job id: state (queued, running, cancelling, done, failed, cancelled), progress, the results so far while it runs, and the full result once done. Answers at once even while the editor is busy; with wait_s it holds the answer until the job finishes (up to 40 s), so you can wait on a long job without a sleep of your own.")]
    async fn job_status(&self, Parameters(p): Parameters<JobWait>) -> CallToolResult {
        self.run("job_status", p).await
    }
    #[tool(description = "Measure the technical quality of many photos at once and rank them: sharpness of the main face (or of the sharpest region when no face is found), overall sharpness, exposure and clipping. Returns the suspected blurry shots, a suggested star rating per photo from best to worst, and with apply_stars writes those ratings. A long job (follow it with job_status). Give one file per shot (the RAW of a RAW+JPEG pair). Use it instead of judging hundreds of photos by eye, then look only at the doubtful ones.")]
    async fn assess_photos(&self, Parameters(p): Parameters<Assess>) -> CallToolResult {
        self.run("assess", p).await
    }
    #[tool(description = "Recover a frozen editor. Only for an editor that is frozen, and it closes the window the user may be working in: never use it just because the bridge is missing (that is [editor_without_bridge]: ask the user to reopen the editor). When status keeps failing with [ui_not_responding] (a GPU out-of-memory error is the usual cause), close MCP RapidRAW and open it again. Photos and edits are safe because they are saved as you go; jobs that were running are lost, so check what finished and repeat the rest. If RapidRAW switched itself to its OpenGL backend after the crash, which closes at the first use on some cards, the backend is put back to automatic. Does nothing when the editor answers, unless force is true.")]
    async fn restart_editor(&self, Parameters(p): Parameters<Restart>) -> CallToolResult {
        let failure = |s: String| CallToolResult::error(vec![ContentBlock::text(s)]);
        if p.force != Some(true) {
            match self.exchange_once("status", json!({})).await {
                Ok(_) => return failure("[editor_healthy] The editor answers, so there is nothing to restart. Pass force true to restart it anyway.".into()),
                Err(m) if m.contains("[stock_rapidraw_open]") || m.contains("[editor_not_installed]") => return failure(m),
                Err(_) => {}
            }
        }
        match self.restart().await {
            Ok(v) => CallToolResult::success(vec![ContentBlock::text(v.to_string())]),
            Err(m) => failure(m),
        }
    }
    #[tool(description = "Make a plan before working on a request that has several steps. Give the goal in your own words (and the folder); the server reads the folder and returns an ordered plan made of these tools, with the real numbers (photos, RAW+JPEG pairs, edits already there), time estimates, warnings and the limits of the tools. Call it first, tell the user the plan, then follow it.")]
    async fn plan_task(&self, Parameters(p): Parameters<PlanTask>) -> CallToolResult {
        let folder = p.folder.clone().unwrap_or_else(|| ".".to_string());
        match self.exchange("inventory", json!({"folder": folder})).await {
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
            Ok(inventory) => {
                self.planned.store(true, Ordering::Relaxed);
                self.report.planned();
                CallToolResult::success(vec![ContentBlock::text(plan::build(&p.goal, &inventory).to_string())])
            }
        }
    }
    #[tool(description = "Stop a queued or running operation by its job id. A running one finishes the step it is on (one photo, one segmentation) and keeps what was already done.")]
    async fn cancel_job(&self, Parameters(p): Parameters<JobRef>) -> CallToolResult {
        self.run("cancel_job", p).await
    }
}
#[rmcp::tool_handler]
impl ServerHandler for Editor {
    async fn initialize(
        &self,
        request: rmcp::model::InitializeRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::InitializeResult, rmcp::ErrorData> {
        let client = &request.client_info;
        self.report.client(format!("{} {}", client.name, client.version));
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Controls a running RapidRAW through its real editor. Paths are relative to the first allowed folder, or absolute inside any allowed folder (see list_folders). \
             Workflow: list_folders, open_folder, list_photos, open_photo, preview_photo, set_adjustments or add_mask, \
             preview_photo again (compare=side_by_side, region for zoomed inspection), mask_overlay for masks, \
             undo if worse, export_photos when done. Never edit without checking the result. \
             For a request with several steps, call plan_task first, tell the user the plan, then follow it. To cull or rank many photos use assess_photos instead of judging each by eye. \
             To give photos the look of a reference image, work like a photographer, not like a filter: (1) analyze_reference and LOOK at the picture: how are its whites, its blacks, its greens, blues and skin; tell the user the recipe. (2) For each photo: compare_to_reference, look at the picture next to the reference, decide what THIS photo needs given its own light (flash at night is not golden hour), apply it with style_photo (global sliders, hsl per colour family, colorGrading, curves), look at the result and correct once or twice. Different photos get different settings; a colour that dominates a photo but barely exists in the reference (green bushes, a coloured wall) must be tamed. (3) review_set to judge the whole set next to the reference and fix the photos that break it. Do not paste one photo's adjustments onto others and do not rely on match_style for the style. \
             Long operations (export_photos, select_dress, select_skin, match_style, assess_photos, AI masks) never time out: if one is still going after ~45 s the reply is a job id with state running or queued, \
             not a result. The work continues on its own, even if you stop waiting. Follow it with job_status (progress, partial results, then the full result; wait_s holds the answer until it finishes, so there is no need to sleep), \
             stop it with cancel_job, and never repeat the call, which would queue the same work twice. status shows what the editor is running. \
             The editor may be the user's own session, open and in use right now: never close, restart or reopen it on your own, and do not open another folder or change the active photo unless asked. \
             When the user says 'my session' or 'what I have open', call status (open folder, active photo, selection, filters) and then get_photo or list_photos, and work on that. \
             If [editor_without_bridge] comes back, the editor is open without the agent bridge: tell the user and ask them to reopen it when convenient; do not restart it yourself. \
             Failures carry a [code] such as [editor_not_open], [ui_not_responding], [ui_reloaded] or [version_mismatch]: it says what to fix. \
             If you cannot see images, use analyze_photo (numbers: color balance, exposure, clipping, mask coverage and the colors inside vs outside a mask) \
             and the analysis block that every preview carries. \
             Selecting a subject, learned the hard way: (1) Brush strokes placed by eye give scalloped edges and gaps; use them only to cut or touch up. \
             (2) An AI mask (ai-text) follows the real outline but often takes more than asked: the face and hair, arms, other people's clothes. \
             (3) Brush edits (add_lines) on an AI component do nothing; to cut something out, add a brush component with mode subtractive inside the same add_mask. \
             (4) For a wedding dress call select_dress (many photos) or add_mask with the ai-dress component: it cleans the AI result with the photo's own colours and leaves only the dress. \
             (5) Never delete a person's own mask before the replacement is checked; use replace_name so there is one mask per photo. \
             (6) Check numbers first (coverage_pct, bbox, removed_pct, masks_total), then look at mask_overlay or the contact sheet only where a row looks off. \
             (7) Photos that show no bride (groom with friends, details) come back not ok: leave them untouched. \
             (8) Photos can be cropped; masks always use the uncropped, oriented image, so a cropped preview shows different coordinates than the ones to send.",
        )
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `rapidraw-mcp.exe --doctor` explains what is wrong; it is not an MCP session.
    if std::env::args().any(|a| a == "--doctor") {
        std::process::exit(doctor::run().await);
    }
    Editor::default().serve(rmcp::transport::stdio()).await?.waiting().await?;
    Ok(())
}
