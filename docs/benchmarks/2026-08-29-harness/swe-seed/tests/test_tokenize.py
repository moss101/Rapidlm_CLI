import unittest

from textlib.tokenize import words


class TestWords(unittest.TestCase):
    def test_basic(self):
        self.assertEqual(words("hello world"), ["hello", "world"])

    def test_punctuation_splits(self):
        self.assertEqual(words("wait, what?!"), ["wait", "what"])

    def test_hyphenated_compound_stays_whole(self):
        self.assertEqual(words("a state-of-the-art idea"), ["a", "state-of-the-art", "idea"])

    def test_internal_apostrophe_stays(self):
        self.assertEqual(words("don't stop"), ["don't", "stop"])

    def test_numbers(self):
        self.assertEqual(words("cairo 2049"), ["cairo", "2049"])

    def test_mixed_sentence(self):
        self.assertEqual(
            words("The pre-flight check failed; don't panic."),
            ["The", "pre-flight", "check", "failed", "don't", "panic"],
        )


if __name__ == "__main__":
    unittest.main()
