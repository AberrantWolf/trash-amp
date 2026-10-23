import unittest

from macos_version import bundle_versions


class MacOSVersionTests(unittest.TestCase):
    def test_alpha_and_beta_bundle_metadata(self):
        for version, expected in (
            ("0.1.0-alpha.1", ("0.1.0", "1.1")),
            ("0.1.0-alpha.99", ("0.1.0", "1.99")),
            ("0.1.0-beta.1", ("0.1.0", "2.1")),
            ("0.1.0-beta.2", ("0.1.0", "2.2")),
            ("1.2.3-beta.99", ("1.2.3", "2.99")),
        ):
            with self.subTest(version=version):
                self.assertEqual(bundle_versions(version), expected)

    def test_build_numbers_increase_across_channels_and_sequences(self):
        versions = ["0.1.0-alpha.1", "0.1.0-alpha.99", "0.1.0-beta.1", "0.1.0-beta.2"]
        builds = [tuple(map(int, bundle_versions(v)[1].split("."))) for v in versions]
        self.assertTrue(all(before < after for before, after in zip(builds, builds[1:])))

    def test_invalid_or_unsupported_versions_fail_closed(self):
        for version in (
            "0.1.0", "0.1.0-rc.1", "v0.1.0-beta.1", "0.1-beta.1",
            "0.1.0-beta.0", "0.1.0-beta.01", "0.1.0-beta.100",
            "0.1.0-beta.1+local", "0.1.0-beta.1.extra", "00.1.0-beta.1",
            "0.1.0-alpha.", "0.1.0-beta.1\n",
        ):
            with self.subTest(version=version), self.assertRaises(ValueError):
                bundle_versions(version)
