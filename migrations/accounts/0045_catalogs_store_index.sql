-- no-transaction
-- An index on the foreign key `catalogs.store` (0037, issue #993): the
-- `ON DELETE RESTRICT` check when a store is removed, and the per-store
-- reads that filter catalogs by store (housekeeping's per-store passes,
-- recovery of a store's half-created catalogs). It is on exactly the foreign
-- key's column. Built CONCURRENTLY, in a file of its own, for the reasons
-- given in 0044.
CREATE INDEX CONCURRENTLY IF NOT EXISTS catalogs_store ON catalogs (store);
