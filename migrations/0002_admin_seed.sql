-- First-run rows matching the Java Liquibase changelog.
--
-- The default account is the publicly documented upstream account
-- `jproxy` / `jproxy@2023`. The stored value is MD5(MD5(password)) because the
-- compiled admin UI hashes the password client-side before POSTing it and the
-- Java service hashes the submitted value again. These are public defaults,
-- not operator secrets. The inserts are keyed by username/key rather than by
-- id, so an existing jproxy database keeps its configured values and the
-- baseline keys are added even if its ids were assigned differently.

INSERT INTO system_user (username, password, role)
SELECT 'jproxy', '765EB35667C4323E7CCB88C94C223202', 'admin'
WHERE NOT EXISTS (SELECT 1 FROM system_user WHERE username = 'jproxy');

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
