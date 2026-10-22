import unittest
import reconnect


class ReconnectEvidence(unittest.TestCase):
    def test_five_rounds_balance_all_positions_including_same_profile_control(self):
        orders=[reconnect.order(i) for i in range(5)]
        self.assertIn("reconnect-same",reconnect.ARMS)
        for position in range(5):
            self.assertEqual({row[position] for row in orders},set(reconnect.ARMS))

    def test_exit_success_is_insufficient_without_complete_file_and_correct_hash(self):
        expected="matching"
        self.assertFalse(reconnect.accept(0,{"outcome":"diagnostic_deadline"},1128375,expected,expected))
        self.assertFalse(reconnect.accept(0,{"outcome":"complete"},100,expected,expected))
        self.assertFalse(reconnect.accept(0,{"outcome":"complete"},1128375,"wrong",expected))
        self.assertTrue(reconnect.accept(0,{"outcome":"complete"},1128375,expected,expected))

    def test_tcp_export_contains_no_process_name_addresses_or_ports(self):
        raw=",interface,state,bytes_in,bytes_out,rx_dupe,rx_ooo,re-tx,rtt_avg,rcvsize,\n" \
            "tcp4 192.168.0.2:12345<->1.1.1.1:443,en0,Established,10000,300,100,200,30,250.5 ms,131072,\n"
        result=reconnect.tcp_summary(raw)
        self.assertEqual(result["max_duplicate_received_bytes"],100)
        self.assertEqual(result["max_out_of_order_received_bytes"],200)
        self.assertEqual(result["max_rtt_ms"],250.5)
        self.assertNotIn("192.168",str(result))
        self.assertNotIn("12345",str(result))


if __name__=="__main__":unittest.main()
