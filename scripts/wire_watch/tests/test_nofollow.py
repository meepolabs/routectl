"""nofollow.py: the final path component is never a followed symlink."""

import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import nofollow  # noqa: E402


class OpenDirTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = self._tmp.name
        self.real = os.path.join(self.root, "real")
        os.mkdir(self.real)
        self.link = os.path.join(self.root, "link")
        os.symlink(self.real, self.link)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def refusal(self, path: str) -> str:
        with self.assertRaises(nofollow.DirRefused) as caught:
            os.close(nofollow.open_dir(path))
        return caught.exception.detail

    def test_refusals(self) -> None:
        plain = os.path.join(self.root, "plain")
        open(plain, "w").close()
        rows = [
            ("link", self.link, nofollow.SYMLINK),
            ("link/", self.link + "/", nofollow.SYMLINK),
            ("link//", self.link + "//", nofollow.SYMLINK),
            ("link/.", self.link + "/.", nofollow.BAD_PATH),
            ("real/..", self.real + "/..", nofollow.BAD_PATH),
            ("empty", "", nofollow.BAD_PATH),
            ("root", "/", nofollow.BAD_PATH),
            ("missing", os.path.join(self.root, "nope"), nofollow.MISSING),
            ("missing parent", os.path.join(self.root, "nope", "x"), nofollow.MISSING),
            ("plain file", plain, nofollow.NOT_A_DIRECTORY),
            ("dangling link", os.path.join(self.root, "dangling"), nofollow.SYMLINK),
        ]
        os.symlink(os.path.join(self.root, "gone"), os.path.join(self.root, "dangling"))
        for name, path, detail in rows:
            with self.subTest(name):
                self.assertEqual(self.refusal(path), detail)

    def test_a_real_directory_opens_however_spelled(self) -> None:
        want = os.stat(self.real).st_ino
        for path in (self.real, self.real + "/", self.real + "//"):
            with self.subTest(path):
                fd = nofollow.open_dir(path)
                try:
                    self.assertEqual(os.fstat(fd).st_ino, want)
                finally:
                    os.close(fd)

    def test_a_symlink_in_the_parent_is_followed(self) -> None:
        os.mkdir(os.path.join(self.real, "inner"))
        fd = nofollow.open_dir(os.path.join(self.link, "inner"))
        os.close(fd)

    @unittest.skipIf(os.geteuid() == 0, "root bypasses directory permissions")
    def test_a_search_only_parent_is_enough(self) -> None:
        os.chmod(self.root, 0o100)
        try:
            fd = nofollow.open_dir(self.real)
            os.close(fd)
            symlink_detail = self.refusal(self.link)
        finally:
            os.chmod(self.root, 0o700)
        self.assertEqual(symlink_detail, nofollow.SYMLINK)

    def test_no_descriptor_is_left_open(self) -> None:
        if not os.path.isdir("/proc/self/fd"):
            self.skipTest("no /proc/self/fd")
        before = len(os.listdir("/proc/self/fd"))
        for path in (self.link, self.link + "/", os.path.join(self.root, "nope")):
            self.refusal(path)
        os.close(nofollow.open_dir(self.real))
        self.assertEqual(len(os.listdir("/proc/self/fd")), before)


if __name__ == "__main__":
    unittest.main()
