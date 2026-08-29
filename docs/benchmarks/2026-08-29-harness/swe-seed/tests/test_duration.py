import unittest

from textlib.duration import format_duration, parse_duration


class TestFormatDuration(unittest.TestCase):
    def test_seconds_only(self):
        self.assertEqual(format_duration(45), "45s")

    def test_minutes_and_seconds(self):
        self.assertEqual(format_duration(125), "2m 5s")

    def test_hours_minutes_seconds(self):
        self.assertEqual(format_duration(3661), "1h 1m 1s")

    def test_zero(self):
        self.assertEqual(format_duration(0), "0s")

    def test_large(self):
        self.assertEqual(format_duration(7325), "2h 2m 5s")

    def test_negative(self):
        self.assertEqual(format_duration(-65), "-1m 5s")
        self.assertEqual(format_duration(-3661), "-1h 1m 1s")


class TestParseDuration(unittest.TestCase):
    def test_parse_round_trip(self):
        for value in (0, 45, 125, 3661, 7325):
            self.assertEqual(parse_duration(format_duration(value)), value)

    def test_parse_negative_round_trip(self):
        for value in (-65, -3661):
            self.assertEqual(parse_duration(format_duration(value)), value)

    def test_parse_bad_token_raises(self):
        with self.assertRaises(ValueError):
            parse_duration("5x")


if __name__ == "__main__":
    unittest.main()
