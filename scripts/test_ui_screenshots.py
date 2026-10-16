"""The visual review matrix must cover complete screen families, not just crops."""

import importlib.util
from pathlib import Path
import sys
import unittest


SPEC = importlib.util.spec_from_file_location(
    "ui_screenshots", Path(__file__).with_name("ui-screenshots.py")
)
REVIEW = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = REVIEW
SPEC.loader.exec_module(REVIEW)


class ReviewMatrixTests(unittest.TestCase):
    def test_camera_kind_matrix_is_opt_in_and_marks_real_and_display_profiles(self):
        self.assertEqual(len(list(REVIEW.matrix())), 61)
        cases = list(REVIEW.matrix(True))
        self.assertEqual(len(cases), 26)
        self.assertEqual(len({case[0] for case in cases}), len(cases))
        typed = [scene for _, scene, *_ in cases if scene.camera]
        self.assertEqual({scene.camera for scene in typed}, {"sim:line", "sim:depth", "sim:microscope", "sim:thermal"})
        profiles = [scene for _, scene, *_ in cases if not scene.camera]
        self.assertTrue(all("kind-scanner" in scene.words or "kind-unknown" in scene.words for scene in profiles))

    def test_only_rejects_empty_or_partially_omitted_selection(self):
        for requested in [["line-features-light"], ["welcome-light", "line-features-light"]]:
            with self.assertRaisesRegex(ValueError, "outside the selected matrix"):
                REVIEW.select_cases(False, requested)
        with self.assertRaisesRegex(ValueError, "outside the selected matrix"):
            REVIEW.select_cases(True, ["welcome-light"])
        self.assertEqual([case[0] for case in REVIEW.select_cases(True, ["line-features-light"])], ["line-features-light"])

    def test_all_matrix_combines_both_complete_matrices_without_duplicate_artifacts(self):
        cases = REVIEW.select_cases(all_screens=True)
        self.assertEqual(len(cases), 95)
        self.assertEqual(len({case[0] for case in cases}), 95)
        self.assertEqual({case[0] for case in cases}, {case[0] for case in REVIEW.matrix()} | {case[0] for case in REVIEW.matrix(True)} | {case[0] for case in REVIEW.extra_state_matrix()})
        self.assertEqual(len(REVIEW.select_cases(only=["welcome-light", "line-features-light"], all_screens=True)), 2)

    def test_every_baseline_screen_has_both_appearances_and_a_unique_artifact(self):
        cases = list(REVIEW.matrix())
        names = [case[0] for case in cases]
        self.assertEqual(len(names), len(set(names)))
        for scene in REVIEW.BASELINE:
            appearances = {
                appearance for _, candidate, appearance, scale, size, group in cases
                if candidate.name == scene.name and group == "baseline"
            }
            self.assertEqual(appearances, {"light", "dark"}, scene.name)

    def test_compact_matrix_uses_the_small_window_and_largest_interface(self):
        compact = [case for case in REVIEW.matrix() if case[5] == "compact"]
        self.assertGreater(len(compact), 0)
        for name, scene, appearance, scale, size, group in compact:
            self.assertEqual(scale, "larger", name)
            self.assertEqual(size, (900, 620), name)
        words = {word for _, scene, *_ in compact for word in scene.words.split(",")}
        self.assertTrue({"sheet-bottom", "inspector-bottom", "peek"} <= words)

    def test_review_includes_missing_sheets_and_plain_and_populated_measurements(self):
        words = {word for _, scene, *_ in REVIEW.matrix() for word in scene.words.split(",")}
        self.assertTrue({
            "network", "destinations-remove", "bucket-editor-advanced", "close-error",
            "settings-credential-error", "analytics-measurements", "analytics-measurements-populated",
        } <= words)


if __name__ == "__main__":
    unittest.main()
