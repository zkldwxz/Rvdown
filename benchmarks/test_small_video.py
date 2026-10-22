import pathlib
import tempfile
import threading
import urllib.request
import unittest
from types import SimpleNamespace

import small_video


class SmallVideoEvidence(unittest.TestCase):
    def test_all_four_profiles_take_each_order_position_once(self):
        orders = [small_video.order(n) for n in range(4)]
        for position in range(4):
            self.assertEqual({row[position] for row in orders}, set(small_video.PROFILES))

    def test_default_threshold_and_tuned_threshold_remain_distinct(self):
        args = SimpleNamespace(rvdown="rvdown", aria2="aria2c", relay_url=None)
        dest = pathlib.Path("/tmp/benchmark-output.mp4")
        default = small_video.command("aria2-default-threshold", "http://127.0.0.1/video", dest, args, True)
        tuned = small_video.command("aria2-1m-threshold", "http://127.0.0.1/video", dest, args, True)
        single = small_video.command("rvdown-1", "http://127.0.0.1/video", dest, args, True)
        self.assertIn("--min-split-size=20M", default)
        self.assertIn("--min-split-size=1M", tuned)
        self.assertEqual(single[single.index("--connections") + 1], "1")

    def test_server_records_actual_head_and_range_separately(self):
        with small_video.TraceFixture(b"actual bytes", False) as server:
            threading.Thread(target=server.serve_forever, daemon=True).start()
            url = f"http://127.0.0.1:{server.server_port}/bytes"
            with urllib.request.urlopen(urllib.request.Request(url, method="HEAD")) as response:
                self.assertEqual(response.status, 200)
            with urllib.request.urlopen(urllib.request.Request(url, headers={"Range": "bytes=2-5"})) as response:
                self.assertEqual(response.read(), b"tual")
            trace = server.stats()["request_trace"]
            self.assertEqual(trace, [{"method":"HEAD", "range":None}, {"method":"GET", "range":"bytes=2-5"}])
            server.shutdown()

    def test_incomplete_rows_never_satisfy_scheduled_denominator(self):
        result = small_video.summary([{"ok":True,"elapsed_s":2.0}], planned=4)
        self.assertEqual(result["planned"], 4)
        self.assertEqual(result["passed"], 1)
        self.assertEqual(result["uncompleted"], 3)


if __name__ == "__main__":
    unittest.main()
