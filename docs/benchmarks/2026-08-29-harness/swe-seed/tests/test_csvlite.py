import unittest

from textlib.csvlite import parse_csv_line


class TestParseCsvLine(unittest.TestCase):
    def test_plain_fields(self):
        self.assertEqual(parse_csv_line("a,b,c"), ["a", "b", "c"])

    def test_quoted_delimiter(self):
        self.assertEqual(parse_csv_line('a,"x,y",b'), ["a", "x,y", "b"])

    def test_escaped_quotes_inside_quotes(self):
        self.assertEqual(parse_csv_line('a,"b""c",d'), ["a", 'b"c', "d"])

    def test_trailing_empty_field_is_preserved(self):
        self.assertEqual(parse_csv_line("a,b,"), ["a", "b", ""])

    def test_empty_fields_are_preserved(self):
        self.assertEqual(parse_csv_line(",x,"), ["", "x", ""])

    def test_quoted_empty_field(self):
        self.assertEqual(parse_csv_line('""'), [""])
        self.assertEqual(parse_csv_line('a,"",b'), ["a", "", "b"])

    def test_custom_delimiter(self):
        self.assertEqual(parse_csv_line("a;b", delimiter=";"), ["a", "b"])


if __name__ == "__main__":
    unittest.main()
