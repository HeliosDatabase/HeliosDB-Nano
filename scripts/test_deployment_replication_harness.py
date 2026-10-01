"""Check fixture auth wiring without launching servers or opening network sockets."""
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import test_deployment_replication as harness


class FixtureAuthTests(unittest.TestCase):
    def test_server_default_password_and_explicit_trust(self):
        for mode in ("password", "trust"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as root:
                fixture = harness.Fixture(Path(root) / "fixture", Path("/fixture/nano"), mode)
                try:
                    with patch.object(harness.subprocess, "Popen") as spawn:
                        fixture.start("primary", 0, 0, "primary")
                        args = spawn.call_args.args[0]
                        self.assertEqual(args[args.index("--auth") + 1], mode)
                        if mode == "password":
                            self.assertEqual(args[args.index("--password") + 1], harness.FIXTURE_PASSWORD)
                        else:
                            self.assertNotIn("--password", args)
                finally:
                    fixture.close()

    def test_default_fixture_mode_is_password(self):
        with tempfile.TemporaryDirectory() as root:
            fixture = harness.Fixture(Path(root) / "fixture", Path("/fixture/nano"))
            self.assertEqual(fixture.auth_mode, "password")
            fixture.close()

    def test_client_uses_matching_fixture_credential(self):
        for mode in ("password", "trust"):
            with self.subTest(mode=mode):
                fixture = Mock(auth_mode=mode)
                with patch.object(harness.psycopg2, "connect") as connect:
                    connect.return_value.poll.return_value = harness.extensions.POLL_OK
                    connection = harness.connect(12345, fixture)
                    self.assertIs(connection, connect.return_value)
                    self.assertEqual(connect.call_args.kwargs["password"],
                                     harness.FIXTURE_PASSWORD if mode == "password" else "")
                    self.assertTrue(connect.call_args.kwargs["async_"])


class ReplayVisibilityHarnessTests(unittest.TestCase):
    def test_warming_uses_original_connection_exact_sql_and_five_observed_reads(self):
        connection, fixture, observations = object(), object(), []
        expected = [[1, "committed-one"], [2, "committed-two"]]

        def observe(actual_connection, sql, actual_fixture, records):
            self.assertIs(actual_connection, connection)
            self.assertIs(actual_fixture, fixture)
            self.assertEqual(sql, "SELECT id, note FROM campaign_replication ORDER BY id")
            records.append({"sql": sql, "rows": expected})
            return expected

        with patch.object(harness, "query", side_effect=observe) as query:
            harness.warm_standby_rows(connection, expected, fixture, observations)
        self.assertEqual(query.call_count, 5)
        self.assertEqual([row["attempt"] for row in observations], [1, 2, 3, 4, 5])
        self.assertTrue(all(row["phase"] == "standby_pre_barrier_cache_warm" for row in observations))

    def test_warming_and_cold_assertions_preserve_failures(self):
        observations = []

        def wrong_rows(connection, sql, fixture, records):
            records.append({"sql": sql, "rows": []})
            return []

        with patch.object(harness, "query", side_effect=wrong_rows):
            with self.assertRaisesRegex(AssertionError, "standby warmup 1"):
                harness.warm_standby_rows(object(), [[1, "committed-one"]], object(), observations)
            with self.assertRaisesRegex(AssertionError, "expected"):
                harness.assert_query_rows(object(), "SELECT COUNT(*) FROM campaign_replication", [[3]],
                                          object(), observations)
        self.assertEqual(len(observations), 2)


if __name__ == "__main__":
    unittest.main()
