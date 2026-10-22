import unittest
from types import SimpleNamespace
import speed_causes

class CauseEvidence(unittest.TestCase):
    def test_five_arms_balance_positions(self):
        for position in range(5):
            self.assertEqual({speed_causes.order(n)[position] for n in range(5)},set(speed_causes.ARMS))
    def test_aria_default_removes_only_custom_ua_and_encoding_headers(self):
        args=SimpleNamespace(aria2='aria2c',rvdown='rvdown',probe='probe',relay_url='http://127.0.0.1:1',work=None)
        from pathlib import Path
        command=speed_causes.command('aria2-default',Path('/tmp/example.mp4'),args)
        self.assertTrue(any(s.startswith('--all-proxy=http://127.0.0.1:') for s in command))
        self.assertFalse(any(s.startswith(('--user-agent=','--header=','--referer=')) for s in command))
        self.assertIn('--no-conf=true',command)
        self.assertIn('--no-netrc=true',command)
        self.assertIn('--max-connection-per-server=1',command)
    def test_native_exit_zero_without_complete_never_passes(self):
        self.assertFalse(speed_causes.accept(0,False,1128375,'x','x',{'outcome':'transport_error'}))
        self.assertFalse(speed_causes.accept(0,True,1128375,'x','x',{'outcome':'complete'}))
        self.assertTrue(speed_causes.accept(0,False,1128375,'x','x',{'outcome':'complete'}))

if __name__=='__main__':unittest.main()
