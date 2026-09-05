import copy
import unittest
from identity_compare import compare


class ComparisonTests(unittest.TestCase):
    def setUp(self):
        self.a = dict(schemaVersion=1, stage="input-identity", fixture="same", files=5,
                      warmup=3, samplesMs=[1, 2, 3, 4, 5], hashes=["same"] * 5)

    def test_identical_samples_have_zero_improvement(self):
        self.assertEqual(compare(self.a, self.a)["reductionPercent"], 0)

    def test_rejects_unmatched_evidence(self):
        for field, value in [("fixture", "other"), ("files", 6), ("hashes", ["other"] * 5),
                             ("samplesMs", [float("nan")] * 5), ("samplesMs", [0] * 5)]:
            b = copy.deepcopy(self.a)
            b[field] = value
            with self.assertRaises(ValueError):
                compare(self.a, b)


if __name__ == "__main__":
    unittest.main()
