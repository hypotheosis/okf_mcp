# Show which store each catalog is on (#1030)

Generated: 2026-11-28T09:26:00Z

An operator could not tell from the admin console or the CLI which store
(and so which database) a catalog lives on. It is now shown, read-only,
because a catalog's store is fixed when it is created.

## Added

- `okf-mcp catalog list` prints `store NAME` after the owner column. The
  description stays last.
- `GET /api/v1/catalogs` and the answer to `POST /api/v1/catalogs` add a
  `store` object (`name`, `location`, `host`, `port`, `database`) to each
  catalog, for an admin credential only. The fields and values are those of
  `GET /api/v1/stores`; no credential kind, reference or user. A key below
  admin, and a provisioner key (which `GET /api/v1/stores` also refuses),
  get no `store` key, so their answer is unchanged. The issue left the
  audience open; admins only is the safe default.
- `unavailable_stores` in `GET /api/v1/catalogs`, for an admin: a map from
  each id in `unavailable` to its store's name. `unavailable` is unchanged.
- `catalogs` in each `GET /api/v1/stores` entry: how many provisioned,
  undeleted catalogs the store holds, for the Stores page (#1031).
- Console, Catalogs page (admin): a Store column (location as the tooltip)
  that sorts, a Store filter, the store and its location in the catalog
  drawer, and a notice naming each unreachable store and its catalogs.

## Tests

- API: `an_admin_sees_each_catalogs_store_and_no_one_else_does`,
  `an_unavailable_catalog_is_named_by_store_for_an_admin_only`,
  `the_stores_list_counts_each_stores_catalogs`.
- CLI: `catalog_list_columns_have_the_documented_order`.
- ui_e2e: `an_admin_sees_which_store_each_catalog_is_on`.
