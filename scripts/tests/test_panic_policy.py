#!/usr/bin/env python3
"""Compile tiny Clippy fixtures against Kit's real config, without dependencies."""
import json
import os
import re
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
LINTS = (
    "unwrap_used", "expect_used", "panic", "unreachable", "todo", "unimplemented",
    "disallowed_methods", "disallowed_macros",
)
POLICY = "#![cfg_attr(not(test), forbid(" + ",".join(
    "clippy::" + lint for lint in LINTS
) + "))]\n"
TEST_ALLOW = "#[allow(" + ",".join(
    "clippy::" + lint for lint in LINTS if lint not in ("todo", "unimplemented")
) + ")]\n"


def main():
    driver = subprocess.check_output(["rustup", "which", "clippy-driver"], text=True).strip()
    # Check policy wiring only, not test layout or production algorithms.
    manifest = (ROOT / "Cargo.toml").read_text().split("[lints.clippy]", 1)[1]
    for lint in LINTS:
        if not re.search(r'^' + re.escape(lint) + r'\s*=\s*"deny"\s*$', manifest, re.MULTILINE):
            raise SystemExit(f"manifest must deny clippy::{lint}")
    literal_policy = re.sub(r"\s+", "", POLICY)
    for root in ("src/lib.rs", "src/main.rs", "build.rs"):
        if literal_policy not in re.sub(r"\s+", "", (ROOT / root).read_text()):
            raise SystemExit(f"{root} must retain the literal production forbid")
    cases = []
    for kind, expression in [
        ("option_unwrap", "value.unwrap()"),
        ("option_expect", 'value.expect("value")'),
        ("option_ufcs", "Option::unwrap(value)"),
        ("option_item", "{ let f = Option::unwrap; f(value) }"),
        ("option_expect_ufcs", 'Option::expect(value, "value")'),
        ("option_expect_item", '{ let f = Option::expect; f(value, "value") }'),
    ]:
        cases.append((kind, f"pub fn check(value: Option<u8>) -> u8 {{ {expression} }}", False, False))
    for method in ("unwrap", "expect", "unwrap_err", "expect_err"):
        argument = ', "value"' if "expect" in method else ""
        for form, expression in [
            ("method", f"value.{method}({argument.removeprefix(', ')})"),
            ("ufcs", f"Result::{method}(value{argument})"),
            ("item", f"{{ let f = Result::{method}; f(value{argument}) }}"),
        ]:
            cases.append((f"result_{method}_{form}", f"pub fn check(value: Result<u8, u8>) -> u8 {{ {expression} }}", False, False))
    macros = {
        "panic": '"failure"', "unreachable": "", "todo": "", "unimplemented": "",
        "assert": "value", "assert_eq": "value, true", "assert_ne": "value, false",
        "debug_assert": "value", "debug_assert_eq": "value, true", "debug_assert_ne": "value, false",
    }
    for macro, args in macros.items():
        for prefix in ("", "core::", "std::"):
            name = prefix.replace("::", "_") + macro
            body = f"pub fn check(value: bool) {{ {prefix}{macro}!({args}); }}"
            cases.append((name, body, False, False))
        cases.append((f"wrapper_{macro}", f"macro_rules! wrapped {{ ($v:expr) => {{ {macro}!({args.replace('value', '$v')}); }} }} pub fn check(value: bool) {{ wrapped!(value); }}", False, False))
    cases.extend([
        ("panic_any", 'pub fn check() { std::panic::panic_any("failure"); }', False, False),
        ("panic_any_item", 'pub fn check() { let panic = std::panic::panic_any::<&str>; panic("failure"); }', False, False),
        ("const_unwrap", "pub const VALUE: u8 = Some(1).unwrap();", False, False),
        ("const_expect", 'pub const VALUE: u8 = Some(1).expect("value");', False, False),
        ("local_allow", '#[allow(clippy::unwrap_used, clippy::disallowed_methods)] pub fn check(value: Option<u8>) -> u8 { value.unwrap() }', False, False),
        ("production_in_test", "pub fn check(value: Option<u8>) -> u8 { Option::unwrap(value) }", True, False),
        ("fallible", "pub fn check(value: Option<u8>) -> Result<u8, &'static str> { value.ok_or(\"missing value\") }", False, True),
        ("test_helpers", "#[cfg(test)] " + TEST_ALLOW + '''mod tests {
            fn helper(value: Option<u8>) -> u8 { Option::unwrap(value) }
            #[test] fn ordinary() {
                assert_eq!(helper(Some(1)), 1);
                assert!(Some(1).is_some());
                let _ = Some(1).expect("value");
                let _ = Result::<u8, u8>::Err(2).unwrap_err();
            }
        }''', True, True),
        ("integration_helper", "#![cfg(test)]\n" + TEST_ALLOW.replace("#[", "#![", 1) + "fn helper(value: Option<u8>) -> u8 { value.unwrap() } #[test] fn ordinary() { assert_eq!(helper(Some(1)), 1); }", True, True),
        ("test_todo", "#[cfg(test)] " + TEST_ALLOW + "mod tests { #[test] fn placeholder() { todo!() } }", True, False),
        ("test_unimplemented", "#[cfg(test)] " + TEST_ALLOW + "mod tests { #[test] fn placeholder() { unimplemented!() } }", True, False),
    ])
    # Document a known test-only lint limitation rather than claiming a proof:
    # test macro allowances also hide placeholders inside local wrappers.
    for macro in ("todo", "unimplemented"):
        cases.append((f"test_wrapper_{macro}_limitation", "#[cfg(test)] " + TEST_ALLOW + f"mod tests {{ macro_rules! later {{ () => {{ {macro}!() }} }} #[test] fn placeholder() {{ later!() }} }}", True, True))
    with tempfile.TemporaryDirectory(prefix="kit-panic-policy-") as temporary:
        directory = Path(temporary)
        for name, source, test, success in cases:
            path = directory / (name + ".rs")
            path.write_text(POLICY + source)
            command = [driver, str(path), "--edition=2024", "--crate-type=lib", "--emit=metadata", "--out-dir", temporary, "--error-format=json"]
            command.extend("-Dclippy::" + lint for lint in LINTS)
            if test:
                command.append("--test")
            result = subprocess.run(command, env={**os.environ, "CLIPPY_CONF_DIR": str(ROOT)}, capture_output=True, text=True)
            diagnostics = [json.loads(line) for line in result.stderr.splitlines() if line.startswith("{")]
            errors = [d for d in diagnostics if d.get("level") == "error"]
            if name == "local_allow":
                expected_code = "E0453"
            elif name == "test_todo":
                expected_code = "clippy::todo"
            elif name == "test_unimplemented":
                expected_code = "clippy::unimplemented"
            elif "wrapper_" in name or name.removeprefix("core_").removeprefix("std_") in macros:
                expected_code = "clippy::disallowed_macros"
            else:
                expected_code = "clippy::disallowed_methods"
            policy_error = any((d.get("code") or {}).get("code") == expected_code for d in errors)
            if (result.returncode == 0) != success or (not success and not policy_error):
                raise SystemExit(f"{name}: unexpected fixture result\n{result.stderr}")
    print(f"panic policy: {len(cases)} positive/negative fixtures passed")


if __name__ == "__main__":
    main()
