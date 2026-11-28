# Create a user with its email, delete only if it has none (#980)

Generated: 2026-11-28T05:20:04Z

A provisioner that creates a user and then sets its email was left holding an
email-less user when the address belonged to an account it could not see, and
had no safe way to undo it: a GET then DELETE lost any email an operator set in
between.

- `POST /api/v1/users` and `okf-mcp user create` (`--email ADDRESS`) take an
  optional `email`. `users::create_with_email` inserts the user, its personal
  team and the address in one transaction. The address is checked by
  `check_email` and written by the one statement `set_email` uses
  (`store_email`), so the two paths cannot disagree. A taken address (any letter
  case, including one a concurrent create or set-email commits first) rolls the
  whole create back: 409, kind `Refused`, message `Another user has this
  email` (`EMAIL_TAKEN_REFUSAL`), identical to what `PUT /users/{name}/email`
  answers. The body is the same for every holder, an admin the caller cannot see
  included, and names no account.
- `DELETE /api/v1/users/{name}?unless_has_email=true` refuses with 409, kind
  `Refused`, message `the user has an email address` (`HAS_EMAIL_REFUSAL`)
  while the user has an address. It is read under the user's row lock, which
  `set_email` also takes, so an address set concurrently is either seen or waits
  and finds the user gone. Unknown parameters and non-boolean values are still
  400.
- Audit: a create with an email records `user.created` then
  `user.email_changed` (`cleared: false`, never the address), as a create
  followed by a set-email does. Permissions are unchanged: a provisioner or an
  admin reaching every catalog.

Reason strings follow the existing convention of a message that a client
matches by its text (as `SHARED_MEMBER_REFUSAL`); no new error variant.

Tests: store (`users_email_atomic_tests.rs`, with lock-wait races through
`test_locks`), REST in `people.rs` and `provisioner_tests.rs`, and a
malformed-name route.

## Review round 1

`create_with_email` wrote the address after `invalidate`, which locks the
`server_state` singleton until the commit, while `set_email` writes the address
first and invalidates after. A `set_email` moving an address onto a user being
created could then wait on the singleton while the create waited on its
uncommitted unique-email entry: a deadlock, which Postgres resolved by aborting
one of them with a database error instead of the success or 409.

The store now splits the address write (`write_email`) from the entry
(`record_email_change`) and the cache bump. Every path that takes both locks
takes the unique-email entry first, the singleton second: `set_email` as before,
and `create_with_email` writes the address before its first `invalidate`. The
audit order is unchanged: `user.created`, then `user.email_changed`. The delete
path only reads the address under the user row lock and takes the singleton last,
and no other code writes `users.email`, so the order is consistent everywhere.

A new test holds the singleton, queues a create and the real `set_email` for one
address behind it, releases it, and requires one success and one 409. On the old
order it fails with a deadlock abort (`Busy`).
