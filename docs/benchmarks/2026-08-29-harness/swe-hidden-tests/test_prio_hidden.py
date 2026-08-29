import unittest

from textlib.prio import PriorityQueue


class TestPrioHidden(unittest.TestCase):
    def test_three_way_tie_is_fifo(self):
        q = PriorityQueue()
        for name in ("n1", "n2", "n3"):
            q.push(name, 7)
        order = [q.pop() for _ in range(3)]
        self.assertEqual(order, ["n1", "n2", "n3"])

    def test_interleaved_priorities_stay_stable(self):
        q = PriorityQueue()
        q.push("a2", 2)
        q.push("b1", 1)
        q.push("c2", 2)
        q.push("d0", 0)
        q.push("e2", 2)
        order = [q.pop() for _ in range(5)]
        self.assertEqual(order, ["d0", "b1", "a2", "c2", "e2"])

    def test_peek_does_not_consume(self):
        q = PriorityQueue()
        q.push("x", 3)
        q.push("y", 1)
        self.assertEqual(q.peek(), "y")
        self.assertEqual(q.peek(), "y")
        self.assertEqual(len(q), 2)


if __name__ == "__main__":
    unittest.main()
