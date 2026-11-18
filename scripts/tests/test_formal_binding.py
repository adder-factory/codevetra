"""Binding must ignore Rust trivia without changing source offsets or accepting drift."""

import importlib.util
from pathlib import Path
import unittest
import tempfile


SPEC = importlib.util.spec_from_file_location(
    "prepare_formal", Path(__file__).resolve().parents[1] / "prepare-formal.py"
)
binding = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(binding)


class MaskTests(unittest.TestCase):
    def test_literals_and_nested_comments_preserve_offsets_and_newlines(self):
        fragments = [
            '// fn hidden() {\n', '/* outer /* nested */ }\n */',
            r'"escaped \" }"', r'r##"quoted "# }"##', r'br#"bytes }"#',
            r'cr"c string }"', "'{'", r"'\n'", r"'\x7b'", r"'\u{7b}'",
        ]
        for fragment in fragments:
            with self.subTest(fragment=fragment):
                source = "before " + fragment + " after"
                expected = "before " + "".join("\n" if c == "\n" else " " for c in fragment) + " after"
                self.assertEqual(binding.code_mask(source), expected)

    def test_lifetimes_and_ordinary_source_remain_intact(self):
        source = "fn borrow<'a>(input: &'a str) -> &'a str { input }"
        self.assertEqual(binding.code_mask(source), source)
        self.assertEqual(binding.code_mask("// final comment"), " " * 16)

    def test_unterminated_tokens_fail_closed(self):
        for source in ['/* unclosed', '/* outer /* nested */', '"unclosed',
                       '"trailing \\', 'r#"unclosed', 'br##"wrong delimiter"#']:
            with self.subTest(source=source), self.assertRaisesRegex(ValueError, "unterminated Rust"):
                binding.code_mask(source)


class BindingTests(unittest.TestCase):
    def test_only_executable_function_supplies_the_unchanged_body(self):
        body = '{ let text = r#"} fn bound(value: u8)"#; /* } */ value }'
        source = '// fn bound(value: u8) -> u8 { 0 }\nfn bound(value: u8) -> u8 ' + body
        self.assertEqual(binding.extract(source, "bound", "fn bound(value: u8) -> u8"), body)

    def test_signature_ambiguity_and_missing_body_stay_errors(self):
        for source, message in [
            ("fn bound(value: u16) -> u8 { 0 }", "signature changed"),
            ("fn bound() {} fn bound() {}", "expected one production function"),
            ("fn bound(value: u8) -> u8 {", "unterminated production body"),
        ]:
            with self.subTest(source=source), self.assertRaisesRegex(ValueError, message):
                binding.extract(source, "bound", "fn bound(value: u8) -> u8")


class ConstantBindingTests(unittest.TestCase):
    def test_scalar_initializer_is_copied_from_production_with_trivia(self):
        declaration = "const CAP: u64 = 8 * 1_048_576 /* ; ignored */;"
        source = '// const CAP: u64 = 0;\npub(super) ' + declaration
        self.assertEqual(binding.extract_constant(source, "CAP", "const CAP: u64"), declaration)

    def test_missing_ambiguous_or_changed_constants_fail_closed(self):
        cases = [
            ("const CAP: u32 = 8;", "signature changed"),
            ("const CAP: u64 = 8; const CAP: u64 = 9;", "expected one production constant"),
            ("// const CAP: u64 = 8;", "expected one production constant"),
            ("const CAP: u64 = 8", "unterminated production constant"),
            ("const CAP: u64 = { let x = 8; x };", "unsupported production constant initializer"),
        ]
        for source, message in cases:
            with self.subTest(source=source), self.assertRaisesRegex(ValueError, message):
                binding.extract_constant(source, "CAP", "const CAP: u64")
        with self.assertRaisesRegex(ValueError, "invalid production constant name"):
            binding.extract_constant("", "CAP.*", "")

    def test_nested_and_conditionally_compiled_constants_cannot_replace_imports(self):
        declarations = [
            '#[cfg(any())] const CAP: usize = 8;',
            '#[cfg_attr(feature = "x", cfg(any()))] const CAP: usize = 8;',
            'mod unrelated { const CAP: usize = 8; }',
            'fn unrelated() { const CAP: usize = 8; }',
            '#![cfg(any())] const OTHER: usize = 0; const CAP: usize = 8;',
        ]
        for declaration in declarations:
            source = declaration + '\nuse crate::limits::CAP; fn bound() -> usize { CAP }'
            with self.subTest(source=source), self.assertRaisesRegex(ValueError, 'module'):
                binding.extract_constant(source, 'CAP', 'const CAP: usize')

    def test_mismatched_module_delimiters_cannot_admit_a_constant(self):
        for prefix in [')', 'mod unrelated {)', 'fn other(]']:
            with self.subTest(prefix=prefix), self.assertRaisesRegex(ValueError, 'unbalanced'):
                binding.extract_constant(prefix + 'const CAP: usize = 8;', 'CAP', 'const CAP: usize')

    def test_prior_items_and_comment_attributes_do_not_change_module_ownership(self):
        source = ('mod unrelated { const OTHER: usize = 9; }\n'
                  '// #[cfg(any())]\n/* #[cfg_attr(any(), any())] */\n'
                  'pub(crate) const CAP: usize = 8;')
        self.assertEqual(binding.extract_constant(source, 'CAP', 'const CAP: usize'),
                         'const CAP: usize = 8;')

    def test_generation_copies_updated_value_and_keeps_independent_contract(self):
        module = {
            "module": "bounded", "source": "source.rs",
            "constants": [{"name": "CAP", "signature": "const CAP: usize"}],
            "functions": [{"name": "bound", "signature": "fn bound() -> usize",
                           "ensures": ["result == 8"]}],
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.rs"
            for value in [8, 9]:
                source.write_text(f"const CAP: usize = {value};\nfn bound() -> usize {{ CAP }}")
                output = binding.generate(root, {"modules": [module]})
                self.assertIn(f"const CAP: usize = {value};", output)
                self.assertIn("result == 8", output)
                self.assertIn("{ CAP }", output)
            module["constants"].append(module["constants"][0])
            with self.assertRaisesRegex(ValueError, "duplicate proof constant"):
                binding.generate(root, {"modules": [module]})


class StructBindingTests(unittest.TestCase):
    DECLARATION = {
        "name": "Position",
        "declaration": "struct Position { byte: u64, line: u32, }",
        "attributes": ["#[derive(Clone, Copy)]"],
    }

    def test_actual_fields_are_copied_and_reviewed_attributes_are_checked(self):
        source = ("// source position\n#[derive(Clone, Copy)]\npub struct Position {\n"
                  "  byte: u64, // exact byte\n  line: u32,\n}")
        self.assertEqual(binding.extract_struct(source, self.DECLARATION),
                         "struct Position {\n  byte: u64, // exact byte\n  line: u32,\n}")

    def test_changed_layout_attributes_scope_and_ambiguity_fail_closed(self):
        original = "#[derive(Clone, Copy)] struct Position { byte: u64, line: u32, }"
        cases = [
            (original.replace("byte: u64", "byte: u32"), "fields changed"),
            (original.replace("line: u32,", "line: u32, column: u32,"), "fields changed"),
            (original.replace("line: u32", "#[cfg(any())] line: u32"), "fields changed"),
            ("#[cfg(any())] " + original, "attributes changed"),
            (original.replace("Clone, Copy", "Clone, Copy, Default"), "attributes changed"),
            ("mod inner { " + original + " }", "file module"),
            ("#![cfg(any())] " + original, "module attributes"),
            (original + original, "expected one production struct"),
        ]
        for source, message in cases:
            with self.subTest(source=source), self.assertRaisesRegex(ValueError, message):
                binding.extract_struct(source, self.DECLARATION)

    def test_verifiers_share_bound_fields_and_fail_on_source_drift(self):
        module = {
            "module": "coordinates", "source": "source.rs",
            "types": [self.DECLARATION],
            "functions": [{"name": "bound", "signature": "fn bound(p: &Position) -> bool",
                           "ensures": ["result <==> p.line > 0"]}],
        }
        manifest = {"modules": [module]}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.rs"
            source.write_text("#[derive(Clone, Copy)] struct Position { byte: u64, line: u32, }\n"
                              "fn bound(p: &Position) -> bool { p.line != 0 }")
            for generate in [binding.generate, binding.generate_kani_types]:
                self.assertIn(self.DECLARATION["declaration"], generate(root, manifest))
            source.write_text(source.read_text().replace("line: u32", "line: u64"))
            for generate in [binding.generate, binding.generate_kani_types]:
                with self.assertRaisesRegex(ValueError, "fields changed"):
                    generate(root, manifest)


if __name__ == "__main__":
    unittest.main()
