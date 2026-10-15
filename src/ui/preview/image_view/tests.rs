// SPDX-License-Identifier: MIT

use super::*;

fn size(width: u32, height: u32) -> ImagePreviewSize {
    ImagePreviewSize { width, height }
}

#[test]
fn a_decode_that_filled_its_box_is_refined_once_the_pane_outgrows_it() {
    let decoded = size(1600, 1000);
    assert_eq!(
        refinement((1500, 1000), decoded, (3000.0, 2000.0)),
        Some(size(3000, 2000))
    );
    assert_eq!(refinement((1500, 1000), decoded, (1600.0, 1050.0)), None);
}

#[test]
fn a_source_smaller_than_its_box_is_never_decoded_again() {
    let decoded = size(1600, 1000);
    assert!(is_native((640, 480), decoded));
    assert_eq!(refinement((640, 480), decoded, (6000.0, 4000.0)), None);
}

#[test]
fn a_decode_already_at_the_pixel_budget_is_not_requested_again() {
    let capped = ImagePreviewSize::new(20_000, 20_000);
    let texture = (capped.width, capped.height);
    assert_eq!(refinement(texture, capped, (40_000.0, 40_000.0)), None);
}
