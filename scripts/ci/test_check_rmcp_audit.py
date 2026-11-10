import copy
import unittest

from check_rmcp_audit import check


class CheckRmcpAuditTest(unittest.TestCase):
    def setUp(self):
        self.metadata = {
            "packages": [{
                "name": "rmcp", "version": "1.6.0", "id": "rmcp-id",
                "source": "registry+https://github.com/rust-lang/crates.io-index",
            }],
            "resolve": {"nodes": [{"id": "rmcp-id", "features": ["client"]}]},
        }

    def test_accepts_only_the_pinned_safe_feature_graph(self):
        check(self.metadata)
        for feature in ("transport-streamable-http-client-reqwest", "transport-streamable-http-client-unix-socket"):
            metadata = copy.deepcopy(self.metadata)
            metadata["resolve"]["nodes"][0]["features"].append(feature)
            with self.subTest(feature=feature), self.assertRaises(ValueError):
                check(metadata)

    def test_changed_version_source_and_missing_node_fail_closed(self):
        for section, key, value in (("packages", "version", "3.9.0"), ("packages", "source", None)):
            metadata = copy.deepcopy(self.metadata)
            metadata[section][0][key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                check(metadata)
        self.metadata["resolve"]["nodes"] = []
        with self.assertRaises(ValueError):
            check(self.metadata)


if __name__ == "__main__":
    unittest.main()
