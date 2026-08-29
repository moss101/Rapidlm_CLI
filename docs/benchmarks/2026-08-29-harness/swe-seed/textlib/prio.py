"""Priority queue: lower priority number pops first, ties pop FIFO."""

import heapq
import itertools


class PriorityQueue:
    """Stable priority queue.

    `pop` returns the entry with the smallest priority number; entries
    with equal priority are returned in insertion order (FIFO).
    """

    def __init__(self):
        self._heap = []
        self._seq = itertools.count()

    def __len__(self):
        return len(self._heap)

    def push(self, item, priority):
        heapq.heappush(self._heap, (priority, -next(self._seq), item))

    def peek(self):
        if not self._heap:
            raise IndexError("peek from empty queue")
        priority, _, item = self._heap[0]
        return item

    def pop(self):
        if not self._heap:
            raise IndexError("pop from empty queue")
        _, _, item = heapq.heappop(self._heap)
        return item
