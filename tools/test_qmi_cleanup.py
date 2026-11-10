"""Offline failure-path tests. Never sends commands to a device."""
import unittest
from unittest.mock import patch
import qmi_dial


class VoicePortTests(unittest.TestCase):
    def test_picks_modem_voice_service_only(self):
        lookup = ("  Service Version Instance Node  Port\n"
                  "       49       1        1    2 16392 IPA control service\n"
                  "        9       2        0    3    64 Voice service\n"
                  "        3       1        0    3    66 Network Access Service\n")
        self.assertEqual(qmi_dial.voice_ports(lookup), [64])
        self.assertEqual(qmi_dial.voice_ports(lookup.replace(" 64 ", " 65 ")), [65])
        self.assertEqual(qmi_dial.voice_ports("        9       2        0    1    64 x\n"), [])


class CleanupTests(unittest.TestCase):
    @patch('qmi_dial.time.sleep')
    @patch('qmi_dial.qmi', side_effect=ConnectionError)
    @patch('qmi_dial.at', side_effect=[ConnectionError(), ConnectionError(), 'OK'])
    def test_transport_failures_still_verify(self, at, qmi, sleep):
        qmi_dial.hangup_and_verify('test', 1)
        qmi.assert_called_once_with('21', bytes([1, 1, 0, 1]))
        self.assertEqual(at.call_count, 3)

    @patch('qmi_dial.time.sleep')
    @patch('qmi_dial.qmi')
    @patch('qmi_dial.at', side_effect=['OK', '', 'NOT OK', '+CLCC: 1,0,0,0,0,"123",129\nOK'])
    def test_incomplete_or_active_never_idle(self, at, qmi, sleep):
        with self.assertRaisesRegex(RuntimeError, 'could not be verified'):
            qmi_dial.hangup_and_verify('test', 1)
        self.assertEqual(at.call_count, 4)

    @patch('qmi_dial.qmi')
    @patch('qmi_dial.at', side_effect=['OK', '+CLCC: 1,0,0,1,0,"",128\nOK'])
    def test_data_bearer_is_not_voice(self, at, qmi):
        qmi_dial.hangup_and_verify('test', None)
        qmi.assert_not_called()


if __name__ == '__main__':
    unittest.main()
