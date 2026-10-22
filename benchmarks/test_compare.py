import importlib.util
import pathlib
import unittest
import struct
import socket

HERE = pathlib.Path(__file__).parent
spec = importlib.util.spec_from_file_location("compare", HERE / "compare.py")
compare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compare)


class BenchmarkInvariants(unittest.TestCase):
    def test_relay_refuses_unknown_hosts_private_addresses_and_non_tls_ports(self):
        self.assertEqual(compare.relay_target("media.example.com:443", {"media.example.com":"1.1.1.1"}), "1.1.1.1")
        for authority, addresses in [("other.example.com:443", {"media.example.com":"1.1.1.1"}),
                                      ("media.example.com:80", {"media.example.com":"1.1.1.1"}),
                                      ("media.example.com:443", {"media.example.com":"198.18.0.1"})]:
            with self.assertRaises(ValueError):
                compare.relay_target(authority, addresses)

    def test_public_inputs_reject_credentials_queries_and_private_hosts(self):
        for url in ["https://user:password@example.com/a", "https://example.com/a?token=x",
                    "https://192.168.0.1/a", "https://198.18.0.1/a", "http://example.com/a"]:
            with self.assertRaises(ValueError):
                compare.public_url(url)
        self.assertEqual(compare.public_url("https://media.example.com/video.mp4"),
                         "https://media.example.com/video.mp4")

    def test_failed_exit_or_bad_hash_never_count_as_success(self):
        self.assertFalse(compare.valid_result(1, 1234, "abc", "abc"))
        self.assertFalse(compare.valid_result(0, 1234, "abc", "def"))
        self.assertFalse(compare.valid_result(0, 0, "abc", "abc"))
        self.assertTrue(compare.valid_result(0, 1234, "abc", "abc"))

    def test_round_order_balances_tool_position(self):
        tools = ["rvdown", "httpx-fetch", "aria2"]
        orders = [compare.round_order(tools, n) for n in range(3)]
        for position in range(3):
            self.assertEqual({order[position] for order in orders}, set(tools))

    def test_summary_keeps_failures_in_denominator_and_uses_mib(self):
        result = compare.summarize([
            {"ok": True, "bytes": 1048576, "elapsed_s": 0.5, "peak_rss_bytes": 20},
            {"ok": False, "bytes": 0, "elapsed_s": 2.0, "peak_rss_bytes": 10},
        ])
        self.assertEqual(result["attempted"], 2)
        self.assertEqual(result["passed"], 1)
        self.assertEqual(result["median_mib_s_successes_only"], 2.0)


if __name__ == "__main__":
    unittest.main()
