CREATE TABLE IF NOT EXISTS chains (
    id  INTEGER PRIMARY KEY,
    url TEXT NOT NULL
);

INSERT OR IGNORE INTO chains (id, url) VALUES
    (1,     'https://ethereum-rpc.publicnode.com'),
    (10,    'https://optimism-rpc.publicnode.com'),
    (8453,  'https://base-rpc.publicnode.com'),
    (42161, 'https://arbitrum-one-rpc.publicnode.com');
