import unittest

from textlib.duration import format_duration, parse_duration


class TestDurationHidden(unittest.TestCase):
    def test_exact_hour(self):
        self.assertEqual(format_duration(3600), "1h")

    def test_exact_minute(self):
        self.assertEqual(format_duration(60), "1m")

    def test_minutes_without_carrying_into_hours(self):
        self.assertEqual(format_duration(3720), "1h 2m")
        self.assertEqual(format_duration(93785), "26h 3m 5s")

    def test_parse_explicit(self):
        self.assertEqual(parse_duration("26h 3m 5s"), 93785)
        self.assertEqual(parse_duration("-2m 30s"), -150)

    def test_parse_zero_and_bad(self):
        self.assertEqual(parse_duration("0s"), 0)
        with self.assertRaises(ValueError):
            parse_duration("")
        with self.assertRaises(ValueError):
            parse_duration("3 h")


if __name__ == "__main__":
    unittest.main()
