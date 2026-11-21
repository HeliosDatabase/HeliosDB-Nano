**`ON DELETE SET DEFAULT`: a default that would keep pointing at the deleted row refuses the delete.**

What was wrong: `ON DELETE SET DEFAULT` set the referencing columns to their default, but when a child row
already held the default and the deleted parent row was the one the default names, the child row was left
pointing at a row that no longer exists.

What happens now: the default must itself satisfy the foreign key. A delete (or a key update, with `ON UPDATE
SET DEFAULT`) that would leave a child row on the vanished key is refused with 23503 and changes nothing.

Tests: `tests/fk_on_delete_set_default.rs` (also covers the action over the PostgreSQL wire, in a transaction
that rolls back, and across a restart).
