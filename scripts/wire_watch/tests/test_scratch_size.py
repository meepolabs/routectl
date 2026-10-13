"""scratch_size.tree_size over trees shaped like what a container can write."""

import os
import subprocess
import sys
import tempfile
import unittest
from typing import List
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import scratch_size  # noqa: E402

CAP = 1 << 30


def write(path: str, size: int) -> None:
    with open(path, "wb") as handle:
        handle.write(b"x" * size)


class TreeSizeTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = self._tmp.name

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_nested_files_are_summed(self) -> None:
        os.makedirs(os.path.join(self.root, "a", "b"))
        write(os.path.join(self.root, "one"), 100_000)
        write(os.path.join(self.root, "a", "b", "two"), 50_000)

        size = scratch_size.tree_size(self.root, CAP)

        self.assertGreaterEqual(size, 150_000)
        self.assertLess(size, 150_000 + 64 * 1024)

    def test_the_root_directory_inode_is_charged(self) -> None:
        for i in range(512):
            write(os.path.join(self.root, "empty-file-with-a-long-name-%04d" % i), 0)
        root_bytes = os.stat(self.root).st_size
        self.assertGreater(root_bytes, 8192, "the fixture did not grow the root directory")

        self.assertEqual(scratch_size.tree_size(self.root, 8192), 8193)

    def test_a_hardlinked_pair_counts_once(self) -> None:
        write(os.path.join(self.root, "one"), 100_000)
        os.link(os.path.join(self.root, "one"), os.path.join(self.root, "two"))

        size = scratch_size.tree_size(self.root, CAP)

        self.assertGreaterEqual(size, 100_000)
        self.assertLess(size, 200_000)

    def test_a_reserved_but_unwritten_extent_counts(self) -> None:
        path = os.path.join(self.root, "reserved")
        write(path, 0)
        done = subprocess.run(["fallocate", "-n", "-l", "8M", path], capture_output=True)
        if done.returncode != 0 or os.stat(path).st_blocks * 512 < 8 << 20:
            self.skipTest("this filesystem does not reserve extents with fallocate -n")
        self.assertEqual(os.stat(path).st_size, 0, "the fixture must reserve without growing the size")

        self.assertGreaterEqual(scratch_size.tree_size(self.root, CAP), 8 << 20)

    def test_a_symlinked_directory_is_not_followed(self) -> None:
        with tempfile.TemporaryDirectory() as outside:
            write(os.path.join(outside, "big"), 1_000_000)
            os.symlink(outside, os.path.join(self.root, "link"))
            os.makedirs(os.path.join(self.root, "sub"))
            os.symlink(outside, os.path.join(self.root, "sub", "link"))
            # Positive control: the same directory reached for real counts.
            self.assertGreaterEqual(scratch_size.tree_size(outside, CAP), 1_000_000)

            self.assertLess(scratch_size.tree_size(self.root, CAP), 1_000_000)

    def test_a_directory_swapped_for_a_link_after_its_stat_is_not_followed(self) -> None:
        sub = os.path.join(self.root, "sub")
        os.makedirs(sub)
        write(os.path.join(sub, "small"), 10)
        real_scandir = os.scandir
        swapped: List[bool] = []

        class SwapAfterStat:
            """`sub` is a directory when the walk stats it and a link to a
            big outside tree by the time the walk descends into it."""

            def __init__(self, item, outside: str) -> None:
                self._item, self._outside = item, outside
                self.name, self.path = item.name, item.path

            def stat(self, *, follow_symlinks: bool = True):
                info = self._item.stat(follow_symlinks=follow_symlinks)
                if self.name == "sub" and not swapped:
                    os.rename(sub, sub + ".moved")
                    os.symlink(self._outside, sub)
                    swapped.append(True)
                return info

        class Listing:
            def __init__(self, target, outside: str) -> None:
                self._listing, self._outside = real_scandir(target), outside

            def __enter__(self):
                return self

            def __exit__(self, *exc) -> None:
                self._listing.close()

            def __iter__(self):
                return (SwapAfterStat(item, self._outside) for item in self._listing)

        with tempfile.TemporaryDirectory() as outside:
            write(os.path.join(outside, "big"), 1_000_000)
            with mock.patch.object(scratch_size.os, "scandir", lambda target: Listing(target, outside)):
                size = scratch_size.tree_size(self.root, CAP)

        self.assertTrue(swapped, "the swap never ran, so nothing was raced")
        self.assertEqual(size, CAP + 1, "a directory that became a link must stop the walk, not be followed")

    def test_over_cap_and_over_the_entry_bound_read_as_over_cap(self) -> None:
        write(os.path.join(self.root, "one"), 10_000)
        self.assertEqual(scratch_size.tree_size(self.root, 100), 101)
        for i in range(5):
            write(os.path.join(self.root, f"empty-{i}"), 0)
        self.assertEqual(scratch_size.tree_size(self.root, CAP, max_entries=3), CAP + 1)

    def test_a_tree_deeper_than_the_bound_reads_as_over_cap(self) -> None:
        path = self.root
        for _ in range(4):
            path = os.path.join(path, "d")
            os.mkdir(path)
        with mock.patch.object(scratch_size, "SIZE_WALK_MAX_DEPTH", 2):
            self.assertEqual(scratch_size.tree_size(self.root, CAP), CAP + 1)
        self.assertLess(scratch_size.tree_size(self.root, CAP), CAP)

    def test_a_symlinked_root_is_refused(self) -> None:
        link = os.path.join(self.root, "link")
        os.symlink(self.root, link)
        self.assertEqual(scratch_size.tree_size(link, CAP), CAP + 1)

    def test_the_entry_budget_is_a_parameter(self) -> None:
        for i in range(10):
            write(os.path.join(self.root, f"e{i}"), 0)
        self.assertEqual(scratch_size.tree_size(self.root, CAP, max_entries=5), CAP + 1)
        self.assertLess(scratch_size.tree_size(self.root, CAP, max_entries=50), CAP)

    def test_a_missing_root_reads_as_over_cap(self) -> None:
        self.assertEqual(scratch_size.tree_size(os.path.join(self.root, "absent"), 10), 11)

    def test_no_descriptor_is_left_open_when_the_walk_fails_deep_inside(self) -> None:
        if not os.path.isdir("/proc/self/fd"):
            self.skipTest("no /proc/self/fd")
        deep = os.path.join(self.root, "a", "b", "c")
        os.makedirs(deep)
        for name in ("x", "y"):
            write(os.path.join(deep, name), 10)
        real_scandir = os.scandir
        unlinked: List[str] = []

        class UnlinkBeforeStat:
            """Lists `c` normally but removes each file before the walk
            stats it, so the stat fails with three child fds open."""

            def __init__(self, target) -> None:
                self._listing = real_scandir(target)

            def __enter__(self):
                return self

            def __exit__(self, *exc) -> None:
                self._listing.close()

            def __iter__(self):
                for item in self._listing:
                    if item.name in ("x", "y"):
                        os.unlink(os.path.join(deep, item.name))
                        unlinked.append(item.name)
                    yield item

        before = len(os.listdir("/proc/self/fd"))
        with mock.patch.object(scratch_size.os, "scandir", UnlinkBeforeStat):
            size = scratch_size.tree_size(self.root, CAP)

        self.assertTrue(unlinked, "nothing was unlinked, so no error path ran")
        self.assertEqual(size, CAP + 1)
        self.assertEqual(len(os.listdir("/proc/self/fd")), before)

    def test_no_descriptor_is_left_open(self) -> None:
        if not os.path.isdir("/proc/self/fd"):
            self.skipTest("no /proc/self/fd")
        os.makedirs(os.path.join(self.root, "a", "b", "c"))
        write(os.path.join(self.root, "a", "b", "c", "f"), 10)
        before = len(os.listdir("/proc/self/fd"))

        scratch_size.tree_size(self.root, CAP)
        scratch_size.tree_size(self.root, 1)

        self.assertEqual(len(os.listdir("/proc/self/fd")), before)


if __name__ == "__main__":
    unittest.main()
