//! UI: the monospace font the window asks for, chosen from the family fontconfig matches for
//! `monospace` and its faces' slants, so code never renders in a mislabelled oblique face (#71).

use std::path::PathBuf;

use akshually_ui::{MonoFace, SYSTEM_MONO, upright_mono};

fn face(path: &str, upright: bool) -> MonoFace {
    MonoFace { path: PathBuf::from(path), upright }
}

#[test]
fn a_family_with_an_oblique_file_labelled_regular_registers_only_its_upright_files() {
    let faces = [
        face("/fonts/BerkeleyMono-Regular.otf", true),
        face("/fonts/BerkeleyMono-Oblique.otf", false),
        face("/fonts/BerkeleyMono-Bold.otf", true),
        face("/fonts/BerkeleyMono-Bold-Oblique.otf", false),
    ];

    let mono = upright_mono(Some("Berkeley Mono"), &faces);

    assert_eq!(mono.slot, "\"Berkeley Mono\", Inter");
    assert_eq!(
        mono.files,
        [PathBuf::from("/fonts/BerkeleyMono-Regular.otf"), PathBuf::from("/fonts/BerkeleyMono-Bold.otf")],
        "the oblique files must not be registered, and bold must be"
    );
}

#[test]
fn a_family_with_no_upright_file_falls_back_to_the_system_monospace() {
    let faces = [face("/fonts/Slanted-Oblique.otf", false)];

    let mono = upright_mono(Some("Slanted"), &faces);

    assert_eq!(mono.slot, SYSTEM_MONO);
    assert!(mono.files.is_empty(), "nothing to register for the fallback, got {:?}", mono.files);
}

#[test]
fn a_family_with_only_upright_faces_registers_all_of_them() {
    let faces = [face("/fonts/Plain-Regular.ttf", true), face("/fonts/Plain-Bold.ttf", true)];

    let mono = upright_mono(Some("Plain"), &faces);

    assert_eq!(mono.slot, "\"Plain\", Inter");
    assert_eq!(mono.files, [PathBuf::from("/fonts/Plain-Regular.ttf"), PathBuf::from("/fonts/Plain-Bold.ttf")]);
}

#[test]
fn no_matched_family_falls_back_to_the_system_monospace() {
    let mono = upright_mono(None, &[]);

    assert_eq!(mono.slot, "ui-monospace, Inter");
    assert!(mono.files.is_empty(), "nothing to register for the fallback, got {:?}", mono.files);
}

#[test]
fn a_collection_file_holding_a_slanted_face_is_not_registered() {
    let faces = [face("/fonts/Mixed.ttc", true), face("/fonts/Mixed.ttc", false), face("/fonts/Mixed-Bold.ttf", true)];

    let mono = upright_mono(Some("Mixed"), &faces);

    assert_eq!(mono.files, [PathBuf::from("/fonts/Mixed-Bold.ttf")], "registering a file registers all its faces");
}

#[test]
fn a_file_with_several_upright_faces_is_registered_once() {
    let faces = [face("/fonts/Upright.ttc", true), face("/fonts/Upright.ttc", true)];

    let mono = upright_mono(Some("Upright"), &faces);

    assert_eq!(mono.files, [PathBuf::from("/fonts/Upright.ttc")]);
}

#[test]
fn a_family_name_with_double_quotes_is_quoted_with_single_ones() {
    let faces = [face("/fonts/Odd-Regular.ttf", true)];

    let mono = upright_mono(Some("Odd \"Mono\""), &faces);

    assert_eq!(mono.slot, "'Odd \"Mono\"', Inter");
}

#[test]
fn a_family_name_with_both_quotes_falls_back_to_the_system_monospace() {
    let faces = [face("/fonts/Odd-Regular.ttf", true)];

    let mono = upright_mono(Some("Odd's \"Mono\""), &faces);

    assert_eq!(mono.slot, SYSTEM_MONO, "a CSS family list can't name it: there are no escapes");
    assert!(mono.files.is_empty(), "nothing to register for the fallback, got {:?}", mono.files);
}
