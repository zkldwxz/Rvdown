"""Offline safeguards for a standalone public SDK; not a complete secret scanner."""
import pathlib
import re
import subprocess
import tomllib
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent


class PublicBoundary(unittest.TestCase):
    def test_path_dependencies_stay_inside_this_workspace(self):
        for file in [ROOT / "Cargo.toml", *ROOT.glob("crates/*/Cargo.toml")]:
            document = tomllib.loads(file.read_text())
            groups = [document.get("workspace", {}).get("dependencies", {})]
            groups.extend(document.get(name, {}) for name in ["dependencies", "dev-dependencies", "build-dependencies"])
            for group in groups:
                for name, entry in group.items():
                    if isinstance(entry, dict) and "path" in entry:
                        path = (file.parent / entry["path"]).resolve()
                        self.assertTrue(path.is_relative_to(ROOT), (file.name, name))

    def test_tracked_files_do_not_contain_obvious_secrets_or_private_config_files(self):
        files = subprocess.check_output(["git", "-C", str(ROOT), "ls-files", "--cached", "--others", "--exclude-standard", "-z"]).split(b"\0")
        credential = re.compile(rb"gh[pousr]_[A-Za-z0-9]{20,}|AKIA[A-Z0-9]{16}|-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----")
        for item in files:
            if not item:
                continue
            relative = pathlib.Path(item.decode())
            self.assertFalse(relative.name == ".env" or relative.name.startswith(".env."), str(relative))
            self.assertNotIn(relative.suffix, [".p12", ".pfx", ".key"], str(relative))
            self.assertIsNone(credential.search((ROOT / relative).read_bytes()), str(relative))


if __name__ == "__main__":
    unittest.main()
