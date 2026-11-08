import hashlib
import importlib.util
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("update_manifest", Path(__file__).with_name("write-update-manifest.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class UpdateManifestTests(unittest.TestCase):
    def test_requires_every_package_and_hashes_the_uploaded_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            with self.assertRaises(ValueError):
                module.manifest("1.4.0", directory)
            names = ["hearthlink-1.4.0-x86_64.tar.gz", "Hearthlink.amd64.deb", "Hearthlink.x86_64.rpm",
                     "Hearthlink-1.4.0-x86_64.AppImage", "Hearthlink-1.4.0-x86_64.flatpak"]
            for name in names:
                (directory / name).write_bytes(b"package contents")
            manifest = module.manifest("1.4.0", directory)
            self.assertEqual(manifest["version"], "1.4.0")
            self.assertEqual([a["name"] for a in manifest["assets"]], names)
            for asset in manifest["assets"]:
                self.assertEqual(asset["sha256"], hashlib.sha256(b"package contents").hexdigest())
                self.assertEqual(asset["size"], len(b"package contents"))
            (directory / names[0]).write_bytes(b"")
            with self.assertRaises(ValueError):
                module.manifest("1.4.0", directory)

    def test_rejects_unsafe_or_prerelease_versions(self):
        for version in ["v1.4.0", "../1.4.0", "1.4", "1.4.0-beta"]:
            with self.assertRaises(ValueError):
                module.manifest(version, Path("."))


if __name__ == "__main__":
    unittest.main()
