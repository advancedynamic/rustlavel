-- One MySQL server serves two suites, because they both wanted port 33306 and
-- only one container can have it.
--
--   rustlavel-db/tests/tls.rs        connects as `rustlavel` to `rustlavel_test`
--                                    (created by MYSQL_DATABASE/MYSQL_USER)
--   rustlavel-db/tests/revocation.rs connects as root to `appdb`, then creates
--                                    and drops a throwaway account inside it
--
-- Only the second database needs making by hand.
CREATE DATABASE IF NOT EXISTS appdb;
