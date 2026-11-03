//! `plan_task`: turns a goal written in plain language into an ordered plan made of this server's own tools.
//! The server reads the folder first (counts, RAW+JPEG pairs, edits already there), so the plan carries real
//! numbers and time estimates instead of generic advice.
use serde_json::{Value, json};

/// Lowercase without accents, so Spanish and English goals match the same words.
fn fold(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' => 'a',
            'é' | 'è' | 'ë' => 'e',
            'í' | 'ì' | 'ï' => 'i',
            'ó' | 'ò' | 'ö' => 'o',
            'ú' | 'ù' | 'ü' => 'u',
            'ñ' => 'n',
            other => other,
        })
        .collect()
}

fn has(text: &str, words: &[&str]) -> bool {
    words.iter().any(|w| text.contains(w))
}

#[derive(Default, Debug, PartialEq)]
pub struct Intents {
    pub cull: bool,
    pub trash: bool,
    pub rate: bool,
    pub style: bool,
    /// The look is to be copied from a photo the user gives, not invented.
    pub reference: bool,
    pub dress: bool,
    pub skin: bool,
    pub export: bool,
}

pub fn intents(goal: &str) -> Intents {
    let g = fold(goal);
    let blur = has(&g, &["desenfoc", "borros", "movid", "blur", "fuera de foco", "out of focus", "nitid", "sharp", "enfoque"]);
    let removal = has(&g, &["elimin", "borr", "quit", "descart", "delete", "remove", "trash", "papelera", "tira"]);
    Intents {
        cull: blur || has(&g, &["cull", "mala calidad", "bad photos", "descart", "seleccion de las mejores"]),
        trash: has(&g, &["papelera", "trash"]) || (removal && (blur || has(&g, &["las fotos", "the photos", "fotos malas"]))),
        rate: has(&g, &["estrella", "puntu", "calific", "rating", "rate ", "stars", "ordena", "mejor a", "best to"]),
        style: has(&g, &["cinemat", "estilo", "look", "edita", "editar", "retoc", "grading", "preset", "filmic", "profesional", " pro", "pro "]),
        reference: has(&g, &["referencia", "reference", "se parezcan", "parecid", "como esta foto", "como la foto", "igual que la foto", "copia el estilo", "copy the look", "match the look", "same look as", "same style as", "estilo de esta foto", "estilo de la foto"]),
        dress: has(&g, &["vestido", "dress", "novia", "bride", "gown"]),
        skin: has(&g, &["piel", "skin", "tez ", "aclar"]),
        export: has(&g, &["export", "entreg", "deliver"]),
    }
}

fn n(inv: &Value, key: &str) -> u64 {
    inv[key].as_u64().unwrap_or(0)
}

fn minutes(seconds: u64) -> String {
    if seconds < 90 {
        format!("about {} s", seconds.max(1))
    } else if seconds < 5400 {
        format!("about {} min", (seconds + 30) / 60)
    } else {
        format!("about {:.1} h", seconds as f64 / 3600.0)
    }
}

pub fn build(goal: &str, inventory: &Value) -> Value {
    let it = intents(goal);
    let images = n(inventory, "images");
    let shots = n(inventory, "distinct_shots").max(1);
    let pairs = n(inventory, "raw_plus_jpeg_pairs");
    let edited = n(inventory, "already_edited");
    let mut steps: Vec<Value> = Vec::new();
    let mut total_s = 0u64;
    let mut warnings: Vec<String> = Vec::new();
    let mut gaps: Vec<String> = Vec::new();

    steps.push(json!({
        "id": "look",
        "title": "Open the folder and make sure nothing is hidden",
        "tools": ["open_folder", "status", "set_filter"],
        "how": format!(
            "open_folder, then compare photos_visible with photos_total. If some are hidden, an old filter is active: set_filter with clear true. The folder holds {images} image files = {shots} distinct shots{}.",
            if pairs > 0 { format!(", {pairs} of them as RAW+JPEG pairs: judge a pair once and give both files the same rating and edit") } else { String::new() }
        ),
        "check": "photos_visible equals photos_total",
    }));

    if it.cull || it.rate {
        let secs = shots * 5;
        total_s += secs;
        steps.push(json!({
            "id": "measure",
            "title": "Measure sharpness and exposure for every shot",
            "tools": ["assess_photos", "job_status"],
            "how": "assess_photos on the shots in batches of at most 150 paths (one file per shot; for a RAW+JPEG pair use the RAW), faces true. It returns the suspected blurry shots, the sharpness of the main face and a suggested star count per photo. Do not judge hundreds of photos by eye: measure, then look only at the doubtful ones.",
            "estimate": minutes(secs),
            "check": "every photo has a row; photos without a detectable face are marked and ranked on their sharpest region",
        }));
    }
    if it.trash {
        steps.push(json!({
            "id": "remove",
            "title": "Remove the blurry shots, only after looking at them",
            "tools": ["preview_photo", "trash_photos"],
            "how": "For each suspect from the measurement, open a zoomed preview of the main face (preview_photo with region, about 100%). Trash only the clearly blurry ones, with an explicit list that includes both files of a RAW+JPEG pair; keep the doubtful ones and say why. Note the reason for each.",
            "check": "list_photos count dropped by exactly the number trashed; the originals outside the work folder are untouched",
        }));
    }
    if it.rate {
        steps.push(json!({
            "id": "rate",
            "title": "Rate the remaining shots from best to worst",
            "tools": ["set_rating", "list_photos"],
            "how": "Use the suggested stars from the measurement (set_rating once per star value with its list of paths), then adjust the top few by eye for expression and composition. Keep a pair on the same rating.",
            "check": "set_filter by rating shows the expected counts per star",
        }));
    }
    if it.reference {
        let secs = shots * 40;
        total_s += secs;
        steps.push(json!({
            "id": "match",
            "title": "Copy the look of the reference photo by photo, judging each one by eye",
            "tools": ["analyze_reference", "compare_to_reference", "style_photo", "review_set", "preview_photo"],
            "how": "The reference must be a file in an allowed folder (an image attached to the chat is not: copy it into the work folder, in a subfolder of its own, and say where). (1) analyze_reference: LOOK at the picture and think how it was made (how are its whites, its blacks, its greens, blues and skin: faded or deep, warm or cool, muted or rich) and tell the user the recipe in plain words. (2) For each photo: compare_to_reference to see it next to the reference with the measured whites, blacks and colour families; decide what THIS photo needs given its own light (flash at night is not golden hour; a bride in white is not a field), apply it with style_photo (global sliders, hsl per colour family, colorGrading, curves; reset true the first time), look at the picture and correct once or twice. Settings differ from photo to photo; a colour that dominates a photo but barely exists in the reference must be tamed. (3) review_set to judge the set as a gallery next to the reference and fix the photos that break it. Do not paste one photo's adjustments onto the others and do not use match_style for the style.",
            "estimate": format!("{} (about 40 s per photo when judged one by one)", minutes(shots * 40)),
            "check": "the sheet from review_set shows photos that belong with the reference and with each other: whites and blacks of the same kind, no colour shouting, nothing that looks like a filter",
        }));
        gaps.push("The look is judged by the agent photo by photo with the tools measuring whites, blacks and colour families; grain, vignette and halation of the reference are not measured (set them by eye). A night flash photo cannot be given golden-hour light: say so instead of forcing it.".into());
        if edited > 0 {
            warnings.push(format!("{edited} photos already carry edits; style_photo with reset true clears only the look (tone, colour, HSL, grading, curves) and keeps masks, crop and detail settings; without reset it merges."));
        }
    }
    if it.style && !it.reference {
        let secs = 20 + shots / 2;
        total_s += secs;
        steps.push(json!({
            "id": "style",
            "title": "Define one look, try it on three different scenes, then apply it to all",
            "tools": ["describe_adjustments", "set_adjustments", "preview_photo", "analyze_photo", "copy_adjustments", "paste_adjustments"],
            "how": if fold(goal).contains("cinemat") {
                "Read describe_adjustments first. A cinematic starting point: gentle S-curve contrast, lifted blacks, recovered highlights, teal toward the shadows and warm orange toward the highlights with colorGrading, slightly lower saturation with some vibrance, a soft vignette and fine grain. Try it on three scenes (bright, indoor flash, low light), compare side_by_side, correct clipped shadows or highlights with analyze_photo, then copy from the best and paste to the rest in batches of 50."
            } else {
                "Read describe_adjustments first, define the look in numbers, try it on three different scenes with preview_photo side_by_side and analyze_photo, then copy from the best and paste to the rest in batches of 50."
            },
            "estimate": minutes(secs),
            "check": "sample at least 10 photos: no clipped highlights, skin not muddy, same look across scenes",
        }));
        if edited > 0 {
            warnings.push(format!("{edited} photos already carry edits; pasting a look merges with or replaces them depending on RapidRAW's paste settings. Check one before pasting to all."));
        }
    }
    if it.dress {
        let secs = shots * 25;
        total_s += secs;
        steps.push(json!({
            "id": "dress",
            "title": "Remove the blue cast from the bride's dress",
            "tools": ["select_dress", "job_status", "mask_overlay"],
            "how": "Do it after the look is applied (local masks sit on top of it). select_dress works on photos with a bride in a white dress and reports ok false for the others: run it on the candidate shots in batches of 20, leave the not-ok photos alone and list them. Retry once the photos that failed with a memory error. Check coverage_pct and bbox in each row; look at mask_overlay only where a row looks off. Pass mode neutral_white to nudge each dress to a neutral white after the mask is made. A photo where no dress is found keeps any mask it already had.",
            "estimate": format!("{} if every shot were a candidate", minutes(secs)),
            "check": "no row with coverage 0 or a bbox spanning the whole frame; the dress is neutral in analyze_photo with mask_id",
        }));
        gaps.push("There is no bride detector: select_dress finds out per photo. Use shots you already know contain her to save time.".into());
    }
    if it.skin {
        let secs = shots * 8;
        total_s += secs;
        steps.push(json!({
            "id": "skin",
            "title": "Brighten skin tones without touching the background",
            "tools": ["select_skin", "job_status", "analyze_photo"],
            "how": "select_skin on the photos with people, in batches of 20: it makes the face and body skin mask, measures the skin inside it and tunes exposure, saturation and warmth in up to three steps toward the target luminance (lift capped at +0.7 EV). Read the rows: a warning says the skin is still dark, pale or grey; look at those with a zoomed preview. A photo whose mask fails keeps its previous skin mask.",
            "estimate": minutes(secs),
            "check": "skin brighter but not pale; hair, background and clothes unchanged",
        }));
        gaps.push("Skin is selected with the AI face model, one mask per photo; very dark, noisy low-light photos can stay grey even after the lift, and the row says so.".into());
    }
    if it.export {
        steps.push(json!({
            "id": "export",
            "title": "Export the result",
            "tools": ["export_photos", "job_status"],
            "how": "export_photos into a subfolder (never the folder with the originals), then job_status until done; the reply lists the files that really landed on disk.",
            "check": "written files match the number of kept shots",
        }));
    }
    steps.push(json!({
        "id": "check",
        "title": "Final check and report",
        "tools": ["status", "list_photos", "preview_photo"],
        "how": "Count what changed (trashed, stars per value, edited, masks), look at a sample, and report what you could not verify.",
        "check": "the numbers in the report match list_photos",
    }));

    if images > 150 && (it.style || it.dress || it.skin) {
        warnings.push(format!("{images} files is a long run for AI work: use batches, follow each with job_status, and expect {} in total.", minutes(total_s)));
    }
    if inventory["raw"].as_u64().unwrap_or(0) > 0 && inventory["other"].as_u64().unwrap_or(0) > 0 && pairs == 0 {
        warnings.push("The folder mixes RAW and JPEG files that are not pairs; treat each file as its own shot.".into());
    }
    if steps.len() == 2 {
        warnings.push("The goal did not mention culling, rating, a look, the dress, skin or export. Only the opening and closing steps are listed; say what you want done and ask again.".into());
    }

    json!({
        "goal": goal,
        "folder": inventory["folder"],
        "inventory": {
            "images": images,
            "distinct_shots": shots,
            "raw_plus_jpeg_pairs": pairs,
            "raw": inventory["raw"],
            "other": inventory["other"],
            "already_edited": edited,
            "size_mb": inventory["size_mb"],
        },
        "steps": steps,
        "estimated_total": minutes(total_s),
        "warnings": warnings,
        "limits_of_the_tools": gaps,
        "rules": [
            "Work on copies. Nothing leaves the allowed folders.",
            "Say the plan to the user before the first change, and report each step when it ends.",
            "A reply with a job id is not a result: follow it with job_status and never repeat the call.",
            "Trash only explicit lists, after looking at each doubtful photo; keep anything doubtful.",
            "Verify with numbers (counts, analyze_photo) and with a sample of previews before saying a step is done.",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory() -> Value {
        json!({"folder": "family", "images": 332, "raw": 201, "other": 131, "distinct_shots": 201,
               "raw_plus_jpeg_pairs": 131, "already_edited": 0, "size_mb": 5248})
    }

    fn ids(plan: &Value) -> Vec<String> {
        plan["steps"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap().to_string()).collect()
    }

    const REQUEST: &str = "elimina las fotos desenfocadas de rostros y las que quedan puntúalas de mejor a menor con estrellas y todas edítalas a nivel pro que tengan un estilo cinematográfico, elimina los azules del vestido y que las pieles sean claras por favor";

    #[test]
    fn the_family_request_is_understood_in_full() {
        let it = intents(REQUEST);
        assert!(it.cull && it.trash && it.rate && it.style && it.dress && it.skin, "{it:?}");
        assert!(!it.export);
    }

    #[test]
    fn the_plan_follows_a_sensible_order() {
        let plan = build(REQUEST, &inventory());
        assert_eq!(ids(&plan), ["look", "measure", "remove", "rate", "style", "dress", "skin", "check"]);
        assert!(plan["estimated_total"].as_str().unwrap().contains("h"), "{}", plan["estimated_total"]);
    }

    #[test]
    fn pairs_and_numbers_reach_the_plan() {
        let plan = build(REQUEST, &inventory());
        let look = plan["steps"][0]["how"].as_str().unwrap();
        assert!(look.contains("201 distinct shots") && look.contains("131"), "{look}");
        assert_eq!(plan["inventory"]["images"], 332);
    }

    #[test]
    fn english_goals_work_too() {
        let it = intents("Cull the blurry photos, rate the rest, and export them");
        assert!(it.cull && it.rate && it.export, "{it:?}");
    }

    #[test]
    fn removing_blue_from_the_dress_is_not_a_request_to_trash_photos() {
        let it = intents("elimina los azules del vestido");
        assert!(it.dress && !it.trash && !it.cull, "{it:?}");
    }

    #[test]
    fn copying_the_look_of_a_photo_plans_the_reference_workflow() {
        let goal = "quiero que todas las fotos se parezcan a esta foto de referencia, con la misma luz y color, y que queden coherentes";
        let it = intents(goal);
        assert!(it.reference, "{it:?}");
        let plan = build(goal, &inventory());
        assert_eq!(ids(&plan), ["look", "match", "check"]);
        let how = plan["steps"][1]["how"].as_str().unwrap();
        assert!(how.contains("analyze_reference") && how.contains("style_photo") && how.contains("review_set"), "{how}");
    }

    #[test]
    fn a_plain_style_request_does_not_ask_for_a_reference() {
        let it = intents("edita todas las fotos con un estilo cinematografico");
        assert!(it.style && !it.reference, "{it:?}");
    }

    #[test]
    fn an_empty_goal_says_so_instead_of_inventing_work() {
        let plan = build("hazlo bonito", &inventory());
        assert_eq!(ids(&plan), ["look", "check"]);
        assert!(!plan["warnings"].as_array().unwrap().is_empty());
    }
}
