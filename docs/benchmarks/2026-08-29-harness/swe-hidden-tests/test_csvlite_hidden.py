import unittest

from textlib.csvlite import parse_csv_line


class TestCsvHidden(unittest.TestCase):
    def test_quoted_delimiter_then_trailing_empty(self):
        self.assertEqual(parse_csv_line('q,"x,y",z,'), ["q", "x,y", "z", ""])

    def test_lone_quoted_empty_between_fields(self):
        self.assertEqual(parse_csv_line('a,"",,'), ["a", "", "", ""])

    def test_escaped_quotes_custom_delimiter(self):
        self.assertEqual(parse_csv_line('a;"b""c"', delimiter=";"), ["a", 'b"c'])

    def test_quotes_mid_field_are_literal(self):
        self.assertEqual(parse_csv_line('a"x,b'), ['a"x', "b"])


if __name__ == "__main__":
    unittest.main()
