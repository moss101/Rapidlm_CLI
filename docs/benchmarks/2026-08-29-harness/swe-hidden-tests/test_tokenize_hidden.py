import unittest

from textlib.tokenize import words


class TestTokenizeHidden(unittest.TestCase):
    def test_multiple_hyphenated_words(self):
        self.assertEqual(
            words("re-enter the co-op now"),
            ["re-enter", "the", "co-op", "now"],
        )

    def test_leading_and_trailing_apostrophes_are_not_part_of_words(self):
        self.assertEqual(words("'tis the season"), ["tis", "the", "season"])
        self.assertEqual(words("the dogs' bowls"), ["the", "dogs", "bowls"])

    def test_hyphen_at_word_edge_splits(self):
        self.assertEqual(words("-start end-"), ["start", "end"])

    def test_mixed_compounds_and_possessives(self):
        self.assertEqual(
            words("well-known fact: it's Bob's co-op"),
            ["well-known", "fact", "it's", "Bob's", "co-op"],
        )


if __name__ == "__main__":
    unittest.main()
