-- Schema compatible with the SQLite database created by upstream jproxy.
-- The tables and columns match src/main/resources/database/changelog in the
-- Java project, including the series_id/movie_id columns added in v3.1.0.
-- No seed rows are inserted: the Rust service never copies upstream secrets.

CREATE TABLE IF NOT EXISTS system_user
(
    id INTEGER NOT NULL PRIMARY KEY,
    username TEXT NOT NULL,
    password TEXT DEFAULT NULL,
    role TEXT DEFAULT 'admin',
    valid_status INTEGER DEFAULT 1 NOT NULL,
    create_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime')),
    update_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime'))
);

CREATE TABLE IF NOT EXISTS system_config
(
    id INTEGER NOT NULL PRIMARY KEY,
    "key" TEXT NOT NULL,
    value TEXT DEFAULT NULL,
    valid_status INTEGER DEFAULT 1 NOT NULL,
    create_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime')),
    update_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime'))
);

CREATE TABLE IF NOT EXISTS sonarr_title
(
    id INTEGER NOT NULL PRIMARY KEY,
    tvdb_id INTEGER NOT NULL,
    sno INTEGER DEFAULT 0 NOT NULL,
    main_title TEXT NOT NULL,
    title TEXT NOT NULL,
    clean_title TEXT NOT NULL,
    season_number INTEGER DEFAULT 1 NOT NULL,
    monitored INTEGER DEFAULT 1 NOT NULL,
    valid_status INTEGER DEFAULT 1 NOT NULL,
    create_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime')),
    update_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime')),
    series_id INTEGER
);

CREATE INDEX IF NOT EXISTS sonarr_title_tvdb_id_idx ON sonarr_title (tvdb_id);
CREATE INDEX IF NOT EXISTS sonarr_title_clean_title_idx ON sonarr_title (clean_title);

CREATE TABLE IF NOT EXISTS tmdb_title
(
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    tvdb_id INTEGER NOT NULL,
    tmdb_id INTEGER DEFAULT NULL,
    language VARCHAR (8) NOT NULL,
    title TEXT NOT NULL,
    valid_status INTEGER DEFAULT 1 NOT NULL,
    create_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime')),
    update_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime'))
);

CREATE INDEX IF NOT EXISTS tmdb_title_tvdb_id_idx ON tmdb_title (tvdb_id);
CREATE INDEX IF NOT EXISTS tmdb_title_tmdb_id_idx ON tmdb_title (tmdb_id);

CREATE TABLE IF NOT EXISTS sonarr_rule
(
    id TEXT NOT NULL PRIMARY KEY,
    token TEXT NOT NULL,
    priority INTEGER DEFAULT 1000 NOT NULL,
    regex TEXT NOT NULL,
    replacement TEXT DEFAULT '' NOT NULL,
    offset INTEGER DEFAULT 0 NOT NULL,
    example TEXT DEFAULT '' NOT NULL,
    remark TEXT DEFAULT NULL,
    author TEXT DEFAULT NULL,
    valid_status INTEGER DEFAULT 1 NOT NULL,
    create_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime')),
    update_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime'))
);

CREATE TABLE IF NOT EXISTS sonarr_example
(
    hash TEXT NOT NULL PRIMARY KEY,
    original_text TEXT NOT NULL,
    format_text TEXT DEFAULT NULL,
    valid_status INTEGER DEFAULT 1 NOT NULL,
    create_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime')),
    update_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime'))
);

CREATE TABLE IF NOT EXISTS radarr_title
(
    id INTEGER NOT NULL PRIMARY KEY,
    tmdb_id INTEGER NOT NULL,
    sno INTEGER DEFAULT 0 NOT NULL,
    main_title TEXT NOT NULL,
    title TEXT NOT NULL,
    clean_title TEXT NOT NULL,
    year INTEGER NOT NULL,
    monitored INTEGER DEFAULT 1 NOT NULL,
    valid_status INTEGER DEFAULT 1 NOT NULL,
    create_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime')),
    update_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime')),
    movie_id INTEGER
);

CREATE TABLE IF NOT EXISTS radarr_rule
(
    id TEXT NOT NULL PRIMARY KEY,
    token TEXT NOT NULL,
    priority INTEGER DEFAULT 1000 NOT NULL,
    regex TEXT NOT NULL,
    replacement TEXT DEFAULT '' NOT NULL,
    offset INTEGER DEFAULT 0 NOT NULL,
    example TEXT DEFAULT '' NOT NULL,
    remark TEXT DEFAULT NULL,
    author TEXT DEFAULT NULL,
    valid_status INTEGER DEFAULT 1 NOT NULL,
    create_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime')),
    update_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime'))
);

CREATE TABLE IF NOT EXISTS radarr_example
(
    hash TEXT NOT NULL PRIMARY KEY,
    original_text TEXT NOT NULL,
    format_text TEXT DEFAULT NULL,
    valid_status INTEGER DEFAULT 1 NOT NULL,
    create_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime')),
    update_time DATETIME DEFAULT (DATETIME(CURRENT_TIMESTAMP, 'localtime'))
);
