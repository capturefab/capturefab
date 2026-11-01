#!/usr/bin/env python3
"""Regression tests for source-driven documentation coverage."""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("docs_coverage", SCRIPTS / "docs-coverage.py")
coverage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(coverage)


def write(root, path, content):
    target = root / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content, encoding="utf-8")


class SourceParserTests(unittest.TestCase):
    def test_enum_reader_ignores_comments_literals_and_nested_fields(self):
        source = '''
            // enum Command { Phantom }
            const EXAMPLE: &str = r##"enum Command { AlsoPhantom }"##;
            /* outside /* nested */ comment */
            pub enum Command {
                #[command(name = "real-name", visible_aliases = ["first", "second"])]
                First { #[arg(help = "commas, braces { } and escaped \\\"quotes\\\"")]
                    choices: Vec<(String, String)> },
                #[cfg(feature = "optional")]
                Optional,
                #[command(hide = true)] Internal,
            }
        '''
        variants = coverage.enum_variants(source, "Command")
        self.assertEqual([v["name"] for v in variants], ["First", "Optional", "Internal"])
        self.assertEqual(coverage.attr_values(variants[0]["attrs"], "command", "name"), ["real-name"])
        self.assertEqual(coverage.attr_values(variants[0]["attrs"], "command", "visible_aliases"), ["first", "second"])

    def test_command_recursion_aliases_hidden_variants_and_feature_gates(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            write(root, "src/cli.rs", '''pub enum Command {
                #[command(visible_alias = "stores")]
                Storage { #[command(subcommand)] command: crate::example_store::Command },
                Setup { #[command(subcommand)] command: Option<SetupCommand> },
                #[command(hide = true)] Internal,
            }''')
            write(root, "src/example_store.rs", '''pub enum Command {
                Init, #[command(hide = true)] Run,
                #[cfg(feature = "optional")] ExportCredentials,
            }''')
            write(root, "src/cli/setup.rs", '''pub enum SetupCommand {
                #[command(name = "inspect", visible_aliases = ["read", "show"])] Describe,
            }''')
            self.assertEqual(coverage.command_paths(root), {
                "help", "storage", "stores", "storage init", "storage export-credentials",
                "setup", "setup inspect", "setup read", "setup show",
            })

    def test_unresolved_child_enum_fails_instead_of_claiming_full_coverage(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            write(root, "src/cli.rs", "enum Command { New { #[command(subcommand)] child: MissingCommand } }")
            with self.assertRaisesRegex(ValueError, "cannot resolve subcommand enum"):
                coverage.command_paths(root)

    def test_rpc_rename_and_worker_only_exclusion(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            write(root, "src/session.rs", '''enum SessionCommand {
                SetMany { values: Vec<String> },
                ReplayCameraChange { token: String },
                #[serde(rename = "special")] DifferentName,
                Grab { timeout_ms: u64 },
            }''')
            self.assertEqual(coverage.rpc_names(root), {"set_many", "replay_camera_change", "special"})

    def test_unterminated_source_fails_closed(self):
        for source in ['/* no end', 'r##"no end', '"no end']:
            with self.subTest(source=source), self.assertRaises(ValueError):
                coverage.rust_tokens(source)

    def test_arguments_include_flattened_structs_tuple_variants_and_globals(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            write(root, "src/cli.rs", '''
                pub struct Cli { #[arg(short='c', long, global=true)] camera: String,
                    #[command(subcommand)] command: Command }
                pub enum Command {
                    Save { #[arg(long="output-file", short='o')] output: String,
                        #[arg(value_name="SOURCE")] path: String,
                        #[arg(hide=true, long)] internal: bool,
                        #[arg(skip)] cache: bool,
                        #[command(flatten)] more: Box<MoreArgs> },
                    Simulate(crate::simulation::RunOptions),
                }
                pub struct MoreArgs { #[cfg(feature="optional")] #[arg(long)] new_feature: bool }
            ''')
            write(root, "src/simulation.rs", 'pub struct RunOptions { #[arg(long)] duration: String }')
            _, arguments = coverage.command_paths(root, with_arguments=True)
            self.assertEqual(arguments, {
                "": {"--camera", "-c"},
                "save": {"--output-file", "-o", "SOURCE", "--new-feature"},
                "simulate": {"--duration"},
            })


    def test_tuple_subcommands_and_flattened_command_enums(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            write(root, "src/cli.rs", '''
                pub struct Cli { #[command(subcommand)] command: Command }
                pub enum Command {
                    #[command(subcommand)] Preset(preset::PresetCommand),
                    #[command(flatten)] Legacy(legacy::LegacyCommand),
                    List(camera::ListArgs),
                }
            ''')
            write(root, "src/cli/preset.rs", 'pub enum PresetCommand { Save { name: String }, List }')
            write(root, "src/cli/legacy.rs", 'pub enum LegacyCommand { Capture { #[arg(short=\'o\')] output: String } }')
            write(root, "src/cli/camera.rs", 'pub struct ListArgs { #[arg(long)] kind: String }')
            commands, arguments = coverage.command_paths(root, with_arguments=True)
            self.assertEqual(commands, {"help", "preset", "preset save", "preset list", "capture", "list"})
            self.assertEqual(arguments["preset"], set())
            self.assertEqual(arguments["preset save"], {"NAME"})
            self.assertEqual(arguments["capture"], {"-o"})
            self.assertEqual(arguments["list"], {"--kind"})


class DocumentationTargetTests(unittest.TestCase):
    def test_heading_needs_explanation_before_next_heading(self):
        page = coverage.DocPage('''<article><h2 id="empty">Empty</h2>
            <h2 id="useful">Capture</h2><p>Save one image.</p></article>''')
        self.assertFalse(page.has_explanation("empty"))
        self.assertTrue(page.has_explanation("useful"))
        self.assertFalse(page.has_explanation("absent"))

    def test_links_and_hidden_text_do_not_count_as_documentation(self):
        page = coverage.DocPage('''<article id="only-links"><h2>Camera</h2>
            <a href="other.html">Read the camera documentation.</a>
            <p aria-hidden="true">This paragraph is hidden.</p></article>
            <table><tr id="row"><td>capture</td><td>Save one image to a file.</td></tr></table>''')
        self.assertFalse(page.has_explanation("only-links"))
        self.assertTrue(page.has_explanation("row"))

    def test_duplicate_anchors_fail(self):
        with self.assertRaisesRegex(ValueError, "duplicate documentation anchor"):
            coverage.DocPage('<p id="one">First</p><p id="one">Second</p>')

    def test_argument_tokens_stay_within_their_command_section(self):
        page = coverage.DocPage('''<article><h3 id="one">One</h3>
            <p>Save with <code>--output-file FILE</code>.</p>
            <h3 id="two">Two</h3><p>Use <code>--new-option</code>.</p></article>''')
        code = page.code_text("one")
        self.assertTrue(coverage.argument_present(code, "--output-file"))
        self.assertTrue(coverage.argument_present(code, "FILE"))
        self.assertFalse(coverage.argument_present(code, "--output"))
        self.assertFalse(coverage.argument_present(code, "--new-option"))


class InventoryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        files = {
            "src/cli.rs": "pub struct Cli {} pub enum Command { One }",
            "src/session.rs": "pub enum SessionCommand { Frame, Grab { timeout_ms: u64 } }",
            "web/reference.html": '''<article><h3 id="one">One</h3><p>Save one image.</p>
                <h3 id="help">Help</h3><p>Show command help.</p>
                <h3 id="frame">Frame</h3><p>Read a preview.</p></article>''',
        }
        for path, text in files.items():
            write(self.root, path, text)
        self.cli = {"schema_version": 1,
                    "commands": {key: f"reference.html#{key}" for key in ("one", "help")},
                    "rpc": {"frame": "reference.html#frame"}}
        self.save()

    def save(self):
        write(self.root, "docs/cli-coverage.json", json.dumps(self.cli))

    def test_full_inventory_passes(self):
        report = coverage.validate(self.root)
        self.assertTrue(report["ok"], report["errors"])
        self.assertEqual(report["coverage"]["commands"], {"documented": 2, "total": 2})

    def test_new_source_command_and_stale_inventory_both_fail(self):
        write(self.root, "src/cli.rs", "pub struct Cli {} pub enum Command { One, NewFeature }")
        self.cli["commands"]["removed"] = "reference.html#one"
        self.save()
        report = coverage.validate(self.root)
        self.assertFalse(report["ok"])
        self.assertIn("new-feature", " ".join(report["errors"]))
        self.assertIn("stale documentation entries: removed", " ".join(report["errors"]))

    def test_missing_and_unexplained_targets_fail(self):
        self.cli["commands"]["one"] = "reference.html#absent"
        self.cli["rpc"]["frame"] = "reference.html#links"
        write(self.root, "web/reference.html", (self.root / "web/reference.html").read_text()
              + '<article id="links"><h2>Frame</h2><a href="other.html">Read more</a></article>')
        self.save()
        report = coverage.validate(self.root)
        self.assertFalse(report["ok"])
        self.assertTrue(any("reference.html#absent" in error for error in report["errors"]))
        self.assertTrue(any("reference.html#links" in error for error in report["errors"]))

    def test_remote_and_traversal_targets_fail(self):
        for target in ("https://example.com/docs#one", "../outside.html#one", "/absolute.html#one"):
            with self.subTest(target=target):
                self.cli["commands"]["one"] = target
                self.save()
                self.assertIn("invalid local documentation target", " ".join(coverage.validate(self.root)["errors"]))

    def test_new_command_option_fails_until_its_code_reference_is_present(self):
        write(self.root, "src/cli.rs", '''pub struct Cli {} pub enum Command {
            One { #[arg(long)] new_option: String, file: String }
        }''')
        report = coverage.validate(self.root)
        self.assertIn("arguments one: missing code reference: --new-option, FILE", " ".join(report["errors"]))
        page = self.root / "web/reference.html"
        page.write_text(page.read_text().replace("Save one image.", "Save one image with <code>--new-option FILE</code>."))
        self.assertTrue(coverage.validate(self.root)["ok"])

    def test_new_global_flag_needs_the_global_code_reference(self):
        write(self.root, "src/cli.rs", '''pub struct Cli { #[arg(long, global=true)] new_global: bool }
            pub enum Command { One }''')
        report = coverage.validate(self.root)
        self.assertIn("arguments (global): missing code reference: --new-global", " ".join(report["errors"]))
        page = self.root / "web/reference.html"
        page.write_text(page.read_text() + '<article id="cli-global-options"><p>Set <code>--new-global</code> for all commands.</p></article>')
        self.assertTrue(coverage.validate(self.root)["ok"])


if __name__ == "__main__":
    unittest.main()
