import unittest

from textlib.prio import PriorityQueue


class TestPriorityQueue(unittest.TestCase):
    def test_orders_by_priority(self):
        q = PriorityQueue()
        q.push("low", 5)
        q.push("high", 1)
        self.assertEqual(q.pop(), "high")
        self.assertEqual(q.pop(), "low")

    def test_ties_pop_in_insertion_order(self):
        q = PriorityQueue()
        q.push("first", 5)
        q.push("second", 5)
        self.assertEqual(q.pop(), "first")
        self.assertEqual(q.pop(), "second")

    def test_len_and_peek(self):
        q = PriorityQueue()
        q.push("a", 2)
        q.push("b", 1)
        self.assertEqual(len(q), 2)
        self.assertEqual(q.peek(), "b")
        q.pop()
        self.assertEqual(len(q), 1)

    def test_mixed_ties_and_priorities(self):
        q = PriorityQueue()
        q.push("p2-a", 2)
        q.push("p1-a", 1)
        q.push("p2-b", 2)
        q.push("p1-b", 1)
        order = [q.pop() for _ in range(4)]
        self.assertEqual(order, ["p1-a", "p1-b", "p2-a", "p2-b"])

    def test_empty_raises(self):
        q = PriorityQueue()
        with self.assertRaises(IndexError):
            q.pop()
        with self.assertRaises(IndexError):
            q.peek()


if __name__ == "__main__":
    unittest.main()
