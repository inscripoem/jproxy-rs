-- First-run rows matching the Java Liquibase changelog.
--
-- The admin user is seeded by `db::seed_first_run_admin` instead of SQL: the
-- default account is `jproxy` / `jproxy@2023`, but `ADMIN_USERNAME` and
-- `ADMIN_PASSWORD` may override it, and an existing `system_user` table is
-- never modified. The stored value is MD5(MD5(password)) because the compiled
-- admin UI hashes the password client-side before POSTing it and the Java
-- service hashes the submitted value again.

WITH defaults("key", value, valid_status) AS (
    VALUES
        ('sonarrUrl', '', 0),
        ('sonarrApikey', '', 0),
        ('sonarrIndexerFormat', '{title} {season}{episode} {language}{subtitle}{resolution}{quality}{dynamic_range}{group}', 1),
        ('sonarrLanguage1', 'zh-CN', 1),
        ('sonarrLanguage2', 'zh-TW', 1),
        ('radarrUrl', '', 0),
        ('radarrApikey', '', 0),
        ('radarrIndexerFormat', '{title} {year} {language}{subtitle}{resolution}{quality}{dynamic_range}{group}', 1),
        ('jackettUrl', '', 0),
        ('prowlarrUrl', '', 0),
        ('qbittorrentUrl', '', 0),
        ('transmissionUrl', '', 0),
        ('tmdbUrl', 'https://api.themoviedb.org', 1),
        ('tmdbApikey', '', 0),
        ('cleanTitleRegex', '(`|,|~|!|@|#|%|&|_|=|''|"|:|<|>|-|—|·|，|～|、|。|‘|’|“|”|？|！|：|（|）|【|】|《|》|♀|/)', 1),
        ('ruleSyncAuthors', 'ALL', 1),
        ('qbittorrentUsername', '', 0),
        ('qbittorrentPassword', '', 0),
        ('transmissionUsername', '', 0),
        ('transmissionPassword', '', 0)
)
INSERT INTO system_config ("key", value, valid_status)
SELECT d."key", d.value, d.valid_status
FROM defaults d
WHERE NOT EXISTS (SELECT 1 FROM system_config s WHERE s."key" = d."key");
