import hashlib
from pathlib import Path
import tempfile
import unittest
import zipfile

import release


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crates/example"]\n[workspace.package]\nversion = "1.2.3"\n',
            encoding="utf-8",
        )
        (self.root / "Cargo.lock").write_text(
            '[[package]]\nname = "example"\nversion = "1.2.3"\n', encoding="utf-8"
        )
        member = self.root / "crates/example"
        member.mkdir(parents=True)
        (member / "Cargo.toml").write_text(
            '[package]\nname = "example"\nversion.workspace = true\n', encoding="utf-8"
        )
        for name in release.DOCUMENTS:
            (self.root / name).write_text(name, encoding="utf-8")

    def build_outputs(self):
        directories = []
        for target in release.TARGETS:
            directory = self.root / "target" / target / "release"
            directory.mkdir(parents=True)
            # Everything a workspace build leaves, the x86 tool included.
            names = ("kinect10.dll", "kinect-winusb.exe", "kinect10.pdb", "kinect_winusb.pdb", "kinect10.dll.lib")
            for name in names:
                (directory / name).write_bytes(target.encode())
            directories.append(directory)
        return directories

    def entries(self, archive):
        with zipfile.ZipFile(archive) as bundle:
            return {name: bundle.read(name) for name in bundle.namelist()}

    def test_tag_matches_workspace_version(self):
        for tag in ("v1.2.3", "1.2.3", ""):
            self.assertFalse(release.validate_tag("1.2.3", tag))
        self.assertTrue(release.validate_tag("1.2.3-rc.1", "v1.2.3-rc.1"))
        self.assertFalse(release.validate_tag("1.2.3+build.4", "v1.2.3+build.4"))
        for version, tag in (("1.2.3", "v1.2.4"), ("01.2.3", ""), ("1.2.3-01", "")):
            with self.subTest(version=version, tag=tag), self.assertRaises(ValueError):
                release.validate_tag(version, tag)

    def test_lockfile_must_match_workspace_version(self):
        self.assertEqual(release.version_info(self.root), "1.2.3")
        (self.root / "Cargo.lock").write_text(
            '[[package]]\nname = "example"\nversion = "1.2.2"\n', encoding="utf-8"
        )
        with self.assertRaises(ValueError):
            release.version_info(self.root)

    def test_archives_contain_only_distribution_files(self):
        self.build_outputs()
        archive, symbols = release.package(self.root, self.root / "dist")
        self.assertEqual(archive.name, "kinect-winusb-1.2.3.zip")
        self.assertEqual(symbols.name, "kinect-winusb-1.2.3-symbols.zip")
        folder = "kinect-winusb-1.2.3/"
        self.assertEqual(
            self.entries(archive),
            {
                folder + "x64/kinect10.dll": b"x86_64-pc-windows-msvc",
                folder + "x64/kinect-winusb.exe": b"x86_64-pc-windows-msvc",
                folder + "x86/kinect10.dll": b"i686-pc-windows-msvc",
                folder + "README.md": b"README.md",
                folder + "README.ja.md": b"README.ja.md",
                folder + "LICENSE": b"LICENSE",
            },
        )
        folder = "kinect-winusb-1.2.3-symbols/"
        self.assertEqual(
            set(self.entries(symbols)),
            {
                folder + "x64/kinect10.pdb",
                folder + "x64/kinect_winusb.pdb",
                folder + "x86/kinect10.pdb",
            },
        )

    def test_missing_file_prevents_archive(self):
        x64, x86 = self.build_outputs()
        for missing in (x64 / "kinect_winusb.pdb", x86 / "kinect10.dll"):
            with self.subTest(missing=missing):
                content = missing.read_bytes()
                missing.unlink()
                with self.assertRaises(FileNotFoundError):
                    release.package(self.root, self.root / "dist")
                self.assertFalse((self.root / "dist").exists())
                missing.write_bytes(content)

    def test_collect_requires_all_archives_and_generates_checksums(self):
        source = self.root / "artifacts"
        source.mkdir()
        names = release.archive_names("1.2.3")
        for name in names:
            (source / name).write_bytes(name.encode())
        output = self.root / "dist"
        release.collect(source, output, "1.2.3")
        lines = (output / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(lines), len(names))
        for line in lines:
            digest, name = line.split("  ")
            self.assertEqual(digest, hashlib.sha256((output / name).read_bytes()).hexdigest())
        (source / names[1]).unlink()
        with self.assertRaises(ValueError):
            release.collect(source, output, "1.2.3")

    def test_collect_rejects_duplicate_artifacts(self):
        source = self.root / "artifacts"
        source.mkdir()
        duplicate = source / "duplicate"
        duplicate.mkdir()
        for name in release.archive_names("1.2.3"):
            (source / name).write_bytes(b"first")
        (duplicate / release.archive_names("1.2.3")[0]).write_bytes(b"second")
        with self.assertRaises(ValueError):
            release.collect(source, self.root / "dist", "1.2.3")


if __name__ == "__main__":
    unittest.main()
