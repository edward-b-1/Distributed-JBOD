"""PKI helper output regression tests, using disposable certificates."""

from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "djbod-pki.sh"


class CertificateListingTests(unittest.TestCase):
    def test_long_names_and_subjects_align_all_certificates(self):
        with tempfile.TemporaryDirectory() as directory:
            def run(*args):
                return subprocess.run(
                    ["bash", str(SCRIPT), "--dir", directory, *args],
                    check=True, capture_output=True, text=True,
                ).stdout

            ca_name = "cluster-authority-with-a-subject-longer-than-forty-columns"
            long_name = "z-client-with-a-name-longer-than-twenty-columns"
            run("init-ca", "--name", ca_name)
            run("client", "admin")
            run("node", long_name, "127.0.0.1,::1")
            output = run("list")
            rows = output.splitlines()
            self.assertEqual(len(rows), 3, output)
            self.assertIn(ca_name, output)
            self.assertIn(long_name, output)
            self.assertIn("IPAddress:127.0.0.1", output)
            self.assertEqual(len({row.index("CN") for row in rows}), 1, output)
            self.assertEqual(len({row.index("until ") for row in rows}), 1, output)


if __name__ == "__main__":
    unittest.main()
