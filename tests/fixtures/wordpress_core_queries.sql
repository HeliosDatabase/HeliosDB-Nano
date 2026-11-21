# Statements WordPress core sends, in the order of a first install and a few page loads:
# connection setup (wpdb), the dbDelta schema of wp-admin/includes/schema.php, the install data,
# options, users, posts, pages, comments, terms, the front-page loop, search, the monthly archive,
# the admin counters, transients, one plugin activation and the export query.
# 
# Assembled by hand from the statement shapes of WordPress 6.x core (wpdb, WP_Query, WP_Term_Query,
# WP_Comment_Query, WP_User_Query, option.php, schema.php). It is NOT a capture of a container run:
# tests/conformance/apps/wordpress/run.sh produces one, and its statement log replaces this file.
# 
# One statement per line: <expectation><TAB><statement>.
#   ok      MySQL runs it and so must this server (no error).
#   ok:N    as ok, and it returns N rows (a result set) or affects N rows (a write).
#   err     MySQL refuses it too (WordPress probes for tables that do not exist yet): must be an error.
#   gap     MySQL runs it; this server does not yet (an error). The test fails when it starts working,
#   gap:N   or MySQL returns/affects N rows and this server answers with another count.
#           so the entry is removed on purpose.
# 
# -- connection setup (wpdb::init_charset, set_charset, set_sql_mode)
ok	SET NAMES 'utf8mb4' COLLATE 'utf8mb4_unicode_520_ci'
ok:1	SELECT @@SESSION.sql_mode
ok	SET SESSION sql_mode='NO_ZERO_IN_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION'
ok:1	SELECT VERSION()
ok:1	SHOW VARIABLES LIKE 'sql_mode'
# -- is WordPress installed? (the tables do not exist yet)
err	SELECT option_value FROM wp_options WHERE option_name = 'siteurl' LIMIT 1
err	DESCRIBE wp_users
ok:0	SHOW TABLES LIKE 'wp_options'
# -- schema (dbDelta)
ok	CREATE TABLE wp_users ( ID bigint(20) unsigned NOT NULL auto_increment, user_login varchar(60) NOT NULL default '', user_pass varchar(255) NOT NULL default '', user_nicename varchar(50) NOT NULL default '', user_email varchar(100) NOT NULL default '', user_url varchar(100) NOT NULL default '', user_registered datetime NOT NULL default '0000-00-00 00:00:00', user_activation_key varchar(255) NOT NULL default '', user_status int(11) NOT NULL default '0', display_name varchar(250) NOT NULL default '', PRIMARY KEY (ID), KEY user_login_key (user_login), KEY user_nicename (user_nicename), KEY user_email (user_email) ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_520_ci
ok	CREATE TABLE wp_usermeta ( umeta_id bigint(20) unsigned NOT NULL auto_increment, user_id bigint(20) unsigned NOT NULL default '0', meta_key varchar(255) default NULL, meta_value longtext, PRIMARY KEY (umeta_id), KEY user_id (user_id), KEY meta_key (meta_key(191)) ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_520_ci
ok	CREATE TABLE wp_termmeta ( meta_id bigint(20) unsigned NOT NULL auto_increment, term_id bigint(20) unsigned NOT NULL default '0', meta_key varchar(255) default NULL, meta_value longtext, PRIMARY KEY (meta_id), KEY term_id (term_id), KEY meta_key (meta_key(191)) ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_520_ci
ok	CREATE TABLE wp_terms ( term_id bigint(20) unsigned NOT NULL auto_increment, name varchar(200) NOT NULL default '', slug varchar(200) NOT NULL default '', term_group bigint(10) NOT NULL default 0, PRIMARY KEY (term_id), KEY slug (slug(191)), KEY name (name(191)) ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_520_ci
ok	CREATE TABLE wp_term_taxonomy ( term_taxonomy_id bigint(20) unsigned NOT NULL auto_increment, term_id bigint(20) unsigned NOT NULL default 0, taxonomy varchar(32) NOT NULL default '', description longtext NOT NULL, parent bigint(20) unsigned NOT NULL default 0, count bigint(20) NOT NULL default 0, PRIMARY KEY (term_taxonomy_id), UNIQUE KEY term_id_taxonomy (term_id,taxonomy), KEY taxonomy (taxonomy) ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_520_ci
ok	CREATE TABLE wp_term_relationships ( object_id bigint(20) unsigned NOT NULL default 0, term_taxonomy_id bigint(20) unsigned NOT NULL default 0, term_order int(11) NOT NULL default 0, PRIMARY KEY (object_id,term_taxonomy_id), KEY term_taxonomy_id (term_taxonomy_id) ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_520_ci
ok	CREATE TABLE wp_commentmeta ( meta_id bigint(20) unsigned NOT NULL auto_increment, comment_id bigint(20) unsigned NOT NULL default '0', meta_key varchar(255) default NULL, meta_value longtext, PRIMARY KEY (meta_id), KEY comment_id (comment_id), KEY meta_key (meta_key(191)) ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_520_ci
ok	CREATE TABLE wp_comments ( comment_ID bigint(20) unsigned NOT NULL auto_increment, comment_post_ID bigint(20) unsigned NOT NULL default '0', comment_author tinytext NOT NULL, comment_author_email varchar(100) NOT NULL default '', comment_author_url varchar(200) NOT NULL default '', comment_author_IP varchar(100) NOT NULL default '', comment_date datetime NOT NULL default '0000-00-00 00:00:00', comment_date_gmt datetime NOT NULL default '0000-00-00 00:00:00', comment_content text NOT NULL, comment_karma int(11) NOT NULL default '0', comment_approved varchar(20) NOT NULL default '1', comment_agent varchar(255) NOT NULL default '', comment_type varchar(20) NOT NULL default 'comment', comment_parent bigint(20) unsigned NOT NULL default '0', user_id bigint(20) unsigned NOT NULL default '0', PRIMARY KEY (comment_ID), KEY comment_post_ID (comment_post_ID), KEY comment_approved_date_gmt (comment_approved,comment_date_gmt), KEY comment_date_gmt (comment_date_gmt), KEY comment_parent (comment_parent), KEY comment_author_email (comment_author_email(10)) ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_520_ci
ok	CREATE TABLE wp_links ( link_id bigint(20) unsigned NOT NULL auto_increment, link_url varchar(255) NOT NULL default '', link_name varchar(255) NOT NULL default '', link_image varchar(255) NOT NULL default '', link_target varchar(25) NOT NULL default '', link_description varchar(255) NOT NULL default '', link_visible varchar(20) NOT NULL default 'Y', link_owner bigint(20) unsigned NOT NULL default '1', link_rating int(11) NOT NULL default '0', link_updated datetime NOT NULL default '0000-00-00 00:00:00', link_rel varchar(255) NOT NULL default '', link_notes mediumtext NOT NULL, link_rss varchar(255) NOT NULL default '', PRIMARY KEY (link_id), KEY link_visible (link_visible) ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_520_ci
ok	CREATE TABLE wp_options ( option_id bigint(20) unsigned NOT NULL auto_increment, option_name varchar(191) NOT NULL default '', option_value longtext NOT NULL, autoload varchar(20) NOT NULL default 'yes', PRIMARY KEY (option_id), UNIQUE KEY option_name (option_name), KEY autoload (autoload) ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_520_ci
ok	CREATE TABLE wp_postmeta ( meta_id bigint(20) unsigned NOT NULL auto_increment, post_id bigint(20) unsigned NOT NULL default '0', meta_key varchar(255) default NULL, meta_value longtext, PRIMARY KEY (meta_id), KEY post_id (post_id), KEY meta_key (meta_key(191)) ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_520_ci
ok	CREATE TABLE wp_posts ( ID bigint(20) unsigned NOT NULL auto_increment, post_author bigint(20) unsigned NOT NULL default '0', post_date datetime NOT NULL default '0000-00-00 00:00:00', post_date_gmt datetime NOT NULL default '0000-00-00 00:00:00', post_content longtext NOT NULL, post_title text NOT NULL, post_excerpt text NOT NULL, post_status varchar(20) NOT NULL default 'publish', comment_status varchar(20) NOT NULL default 'open', ping_status varchar(20) NOT NULL default 'open', post_password varchar(255) NOT NULL default '', post_name varchar(200) NOT NULL default '', to_ping text NOT NULL, pinged text NOT NULL, post_modified datetime NOT NULL default '0000-00-00 00:00:00', post_modified_gmt datetime NOT NULL default '0000-00-00 00:00:00', post_content_filtered longtext NOT NULL, post_parent bigint(20) unsigned NOT NULL default '0', guid varchar(255) NOT NULL default '', menu_order int(11) NOT NULL default '0', post_type varchar(20) NOT NULL default 'post', post_mime_type varchar(100) NOT NULL default '', comment_count bigint(20) NOT NULL default '0', PRIMARY KEY (ID), KEY post_name (post_name(191)), KEY type_status_date (post_type,post_status,post_date,ID), KEY post_parent (post_parent), KEY post_author (post_author) ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_520_ci
ok:12	SHOW TABLES LIKE 'wp\_%'
ok:10	DESCRIBE wp_users
ok:4	SHOW FULL COLUMNS FROM `wp_options`
ok	SHOW INDEX FROM wp_posts
# -- options (populate_options, add_option, update_option, get_option, wp_load_alloptions)
ok:4	INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('siteurl', 'http://localhost:8080', 'yes'), ('home', 'http://localhost:8080', 'yes'), ('blogname', 'My Site', 'yes'), ('blogdescription', '', 'yes')
ok:1	INSERT INTO `wp_options` (`option_name`, `option_value`, `autoload`) VALUES ('active_plugins', 'a:0:{}', 'yes') ON DUPLICATE KEY UPDATE `option_name` = VALUES(`option_name`), `option_value` = VALUES(`option_value`), `autoload` = VALUES(`autoload`)
ok	INSERT INTO `wp_options` (`option_name`, `option_value`, `autoload`) VALUES ('blogname', 'Nano test site', 'yes') ON DUPLICATE KEY UPDATE `option_name` = VALUES(`option_name`), `option_value` = VALUES(`option_value`), `autoload` = VALUES(`autoload`)
ok:1	SELECT option_value FROM wp_options WHERE option_name = 'blogname' LIMIT 1
ok:5	SELECT option_name, option_value FROM wp_options WHERE autoload IN ( 'yes', 'on', 'auto-on', 'auto' )
ok:1	SELECT autoload FROM wp_options WHERE option_name = 'siteurl'
ok:1	UPDATE `wp_options` SET `option_value` = 'Just another WordPress site' WHERE `option_name` = 'blogdescription'
ok:1	INSERT INTO `wp_options` (`option_name`, `option_value`, `autoload`) VALUES ('_transient_doing_cron', '1763632800.1234560012817382812500', 'yes') ON DUPLICATE KEY UPDATE `option_name` = VALUES(`option_name`), `option_value` = VALUES(`option_value`), `autoload` = VALUES(`autoload`)
ok:1	INSERT INTO `wp_options` (`option_name`, `option_value`, `autoload`) VALUES ('_transient_timeout_feed_x', '1700000000', 'off') ON DUPLICATE KEY UPDATE `option_name` = VALUES(`option_name`), `option_value` = VALUES(`option_value`), `autoload` = VALUES(`autoload`)
ok:1	INSERT INTO `wp_options` (`option_name`, `option_value`, `autoload`) VALUES ('_transient_feed_x', 'cached', 'off') ON DUPLICATE KEY UPDATE `option_name` = VALUES(`option_name`), `option_value` = VALUES(`option_value`), `autoload` = VALUES(`autoload`)
ok:1	DELETE FROM `wp_options` WHERE `option_name` = '_transient_doing_cron'
ok:2	DELETE a, b FROM wp_options a, wp_options b WHERE a.option_name LIKE '\_transient\_%' AND a.option_name NOT LIKE '\_transient\_timeout\_%' AND b.option_name = CONCAT( '_transient_timeout_', SUBSTRING( a.option_name, 12 ) ) AND b.option_value < 1763632800
ok:0	SELECT option_value FROM wp_options WHERE option_name = 'cron' LIMIT 1
# -- users (wp_insert_user, get_user_by, update_user_meta, count_users, WP_User_Query)
ok:1	INSERT INTO `wp_users` (`user_pass`, `user_nicename`, `user_email`, `user_url`, `user_registered`, `user_activation_key`, `display_name`, `user_login`) VALUES ('$P$BtestHashtestHashtestHashtestH0', 'admin', 'admin@example.com', '', '2026-11-20 10:00:00', '', 'admin', 'admin')
ok:1	SELECT * FROM wp_users WHERE user_login = 'admin' LIMIT 1
ok:1	SELECT * FROM wp_users WHERE ID = '1' LIMIT 1
ok:0	SELECT * FROM wp_users WHERE user_email = 'nobody@example.com' LIMIT 1
ok:0	SELECT umeta_id FROM wp_usermeta WHERE meta_key = 'nickname' AND user_id = 1
ok:1	INSERT INTO `wp_usermeta` (`user_id`, `meta_key`, `meta_value`) VALUES (1, 'nickname', 'admin')
ok:1	INSERT INTO `wp_usermeta` (`user_id`, `meta_key`, `meta_value`) VALUES (1, 'wp_capabilities', 'a:1:{s:13:"administrator";b:1;}')
ok:1	INSERT INTO `wp_usermeta` (`user_id`, `meta_key`, `meta_value`) VALUES (1, 'wp_user_level', '10')
ok:3	SELECT user_id, meta_key, meta_value FROM wp_usermeta WHERE user_id IN (1) ORDER BY umeta_id ASC
ok:1	UPDATE `wp_usermeta` SET `meta_value` = 'Admin' WHERE `user_id` = 1 AND `meta_key` = 'nickname'
ok:1	SELECT COUNT(NULLIF(`meta_value` LIKE '%\"administrator\"%', false)), COUNT(NULLIF(`meta_value` = 'a:0:{}', false)), COUNT(*) FROM wp_usermeta INNER JOIN wp_users ON user_id = ID WHERE meta_key = 'wp_capabilities'
ok:1	SELECT SQL_CALC_FOUND_ROWS wp_users.ID FROM wp_users WHERE 1=1 ORDER BY user_login ASC LIMIT 0, 20
ok:1	SELECT FOUND_ROWS()
# -- terms (wp_insert_term, wp_set_object_terms, WP_Term_Query, wp_update_term_count)
ok:1	INSERT INTO `wp_terms` (`name`, `slug`, `term_group`) VALUES ('Uncategorized', 'uncategorized', 0)
ok:1	INSERT INTO `wp_term_taxonomy` (`term_id`, `taxonomy`, `description`, `parent`, `count`) VALUES (1, 'category', '', 0, 0)
ok:1	SELECT t.term_id FROM wp_terms AS t INNER JOIN wp_term_taxonomy AS tt ON t.term_id = tt.term_id WHERE tt.taxonomy IN ('category') AND t.slug IN ('uncategorized') ORDER BY t.name ASC LIMIT 1
ok:1	SELECT t.*, tt.* FROM wp_terms AS t INNER JOIN wp_term_taxonomy AS tt ON t.term_id = tt.term_id WHERE t.term_id IN ( 1 )
ok:0	SELECT term_id, meta_key, meta_value FROM wp_termmeta WHERE term_id IN (1) ORDER BY meta_id ASC
# -- posts and pages (wp_insert_post, wp_unique_post_slug, get_post, WP_Query)
ok:1	INSERT INTO `wp_posts` (`post_author`, `post_date`, `post_date_gmt`, `post_content`, `post_content_filtered`, `post_title`, `post_excerpt`, `post_status`, `post_type`, `comment_status`, `ping_status`, `post_password`, `post_name`, `to_ping`, `pinged`, `post_modified`, `post_modified_gmt`, `post_parent`, `menu_order`, `post_mime_type`, `guid`) VALUES (1, '2026-11-20 10:00:00', '2026-11-20 10:00:00', '<!-- wp:paragraph -->\n<p>Welcome to WordPress. This is your first post. Edit or delete it, then start writing!</p>\n<!-- /wp:paragraph -->', '', 'Hello world!', '', 'publish', 'post', 'open', 'open', '', 'hello-world', '', '', '2026-11-20 10:00:00', '2026-11-20 10:00:00', 0, 0, '', 'http://localhost:8080/?p=1')
ok:1	INSERT INTO `wp_posts` (`post_author`, `post_date`, `post_date_gmt`, `post_content`, `post_content_filtered`, `post_title`, `post_excerpt`, `post_status`, `post_type`, `comment_status`, `ping_status`, `post_password`, `post_name`, `to_ping`, `pinged`, `post_modified`, `post_modified_gmt`, `post_parent`, `menu_order`, `post_mime_type`, `guid`) VALUES (1, '2026-11-20 10:00:00', '2026-11-20 10:00:00', 'This is an example page. It''s different from a blog post.', '', 'Sample Page', '', 'publish', 'page', 'closed', 'open', '', 'sample-page', '', '', '2026-11-20 10:00:00', '2026-11-20 10:00:00', 0, 0, '', 'http://localhost:8080/?page_id=2')
ok:1	INSERT INTO `wp_posts` (`post_author`, `post_date`, `post_date_gmt`, `post_content`, `post_content_filtered`, `post_title`, `post_excerpt`, `post_status`, `post_type`, `comment_status`, `ping_status`, `post_password`, `post_name`, `to_ping`, `pinged`, `post_modified`, `post_modified_gmt`, `post_parent`, `menu_order`, `post_mime_type`, `guid`) VALUES (1, '2026-10-05 08:30:00', '2026-10-05 08:30:00', 'An older entry about databases.', '', 'Second post', '', 'publish', 'post', 'open', 'open', '', 'second-post', '', '', '2026-10-05 08:30:00', '2026-10-05 08:30:00', 0, 0, '', 'http://localhost:8080/?p=3')
ok:0	SELECT post_name FROM wp_posts WHERE post_name = 'hello-world' AND post_type = 'post' AND ID != 1 LIMIT 1
ok:1	SELECT * FROM wp_posts WHERE ID = 1 LIMIT 1
ok:1	INSERT INTO `wp_term_relationships` (`object_id`, `term_taxonomy_id`) VALUES (1, 1)
ok:1	INSERT INTO `wp_term_relationships` (`object_id`, `term_taxonomy_id`) VALUES (3, 1)
ok:1	SELECT COUNT(*) FROM wp_term_relationships, wp_posts WHERE wp_posts.ID = wp_term_relationships.object_id AND post_status IN ('publish') AND post_type IN ('post') AND term_taxonomy_id = 1
ok:1	UPDATE `wp_term_taxonomy` SET `count` = 2 WHERE `term_taxonomy_id` = 1
ok:1	INSERT INTO `wp_postmeta` (`post_id`, `meta_key`, `meta_value`) VALUES (2, '_wp_page_template', 'default')
ok:1	INSERT INTO `wp_postmeta` (`post_id`, `meta_key`, `meta_value`) VALUES (1, '_edit_lock', '1763632800:1')
ok:1	SELECT meta_id FROM wp_postmeta WHERE meta_key = '_edit_lock' AND post_id = 1
ok:1	UPDATE `wp_postmeta` SET `meta_value` = '1763632900:1' WHERE `post_id` = 1 AND `meta_key` = '_edit_lock'
ok:2	SELECT post_id, meta_key, meta_value FROM wp_postmeta WHERE post_id IN (1,2) ORDER BY meta_id ASC
# -- the front page (WP_Query main loop, term and meta caches, adjacent post)
ok:2	SELECT SQL_CALC_FOUND_ROWS wp_posts.ID FROM wp_posts WHERE 1=1 AND ((wp_posts.post_type = 'post' AND (wp_posts.post_status = 'publish' OR wp_posts.post_status = 'private'))) ORDER BY wp_posts.post_date DESC LIMIT 0, 10
ok:1	SELECT FOUND_ROWS()
ok:2	SELECT wp_posts.* FROM wp_posts WHERE ID IN (1,3)
ok:2	SELECT t.*, tt.*, tr.object_id FROM wp_terms AS t INNER JOIN wp_term_taxonomy AS tt ON t.term_id = tt.term_id INNER JOIN wp_term_relationships AS tr ON tr.term_taxonomy_id = tt.term_taxonomy_id WHERE tt.taxonomy IN ('category', 'post_tag', 'post_format') AND tr.object_id IN (1, 3) ORDER BY t.name ASC
ok:1	SELECT p.ID FROM wp_posts AS p WHERE p.post_date < '2026-11-20 10:00:00' AND p.post_type = 'post' AND p.post_status = 'publish' ORDER BY p.post_date DESC LIMIT 1
ok:1	SELECT ID, post_name, post_parent, post_type FROM wp_posts WHERE post_name IN ('sample-page') AND post_type IN ('page','attachment')
ok:1	SELECT * FROM wp_posts WHERE (post_type = 'page' AND post_status = 'publish') ORDER BY menu_order,wp_posts.post_title ASC
ok:2	SELECT wp_posts.ID FROM wp_posts WHERE 1=1 AND wp_posts.ID IN (3,1) AND wp_posts.post_type = 'post' ORDER BY FIELD(wp_posts.ID,3,1)
ok:1	SELECT wp_posts.ID FROM wp_posts WHERE 1=1 AND wp_posts.post_type = 'post' AND wp_posts.post_status = 'publish' ORDER BY RAND() LIMIT 0, 1
ok:1	SELECT wp_posts.ID FROM wp_posts INNER JOIN wp_postmeta ON ( wp_posts.ID = wp_postmeta.post_id ) WHERE 1=1 AND ( wp_postmeta.meta_key = '_edit_lock' ) AND wp_posts.post_type = 'post' GROUP BY wp_posts.ID ORDER BY CAST(wp_postmeta.meta_value AS SIGNED) DESC LIMIT 0, 10
# -- search (WP_Query s=hello: MySQL compares without case under WordPress's collation)
ok:1	SELECT SQL_CALC_FOUND_ROWS wp_posts.ID FROM wp_posts WHERE 1=1 AND (((wp_posts.post_title LIKE '%hello%') OR (wp_posts.post_excerpt LIKE '%hello%') OR (wp_posts.post_content LIKE '%hello%'))) AND ((wp_posts.post_type IN ('post', 'page', 'attachment') AND (wp_posts.post_status = 'publish'))) ORDER BY wp_posts.post_title LIKE '%hello%' DESC, wp_posts.post_date DESC LIMIT 0, 10
ok:1	SELECT SQL_CALC_FOUND_ROWS wp_posts.ID FROM wp_posts WHERE 1=1 AND (((wp_posts.post_title LIKE '%Hello%') OR (wp_posts.post_excerpt LIKE '%Hello%') OR (wp_posts.post_content LIKE '%Hello%'))) AND ((wp_posts.post_type IN ('post', 'page', 'attachment') AND (wp_posts.post_status = 'publish'))) ORDER BY wp_posts.post_title LIKE '%Hello%' DESC, wp_posts.post_date DESC LIMIT 0, 10
# -- the monthly archive (wp_get_archives) and a date archive
ok:2	SELECT YEAR(post_date) AS `year`, MONTH(post_date) AS `month`, count(ID) as posts FROM wp_posts WHERE post_type = 'post' AND post_status = 'publish' GROUP BY YEAR(post_date), MONTH(post_date) ORDER BY post_date DESC
ok:1	SELECT SQL_CALC_FOUND_ROWS wp_posts.ID FROM wp_posts WHERE 1=1 AND ( ( YEAR( wp_posts.post_date ) = 2026 AND MONTH( wp_posts.post_date ) = 10 ) ) AND ((wp_posts.post_type = 'post' AND (wp_posts.post_status = 'publish'))) ORDER BY wp_posts.post_date DESC LIMIT 0, 10
# -- comments (wp_insert_comment, WP_Comment_Query, wp_update_comment_count)
ok:1	INSERT INTO `wp_comments` (`comment_post_ID`, `comment_author`, `comment_author_email`, `comment_author_url`, `comment_author_IP`, `comment_date`, `comment_date_gmt`, `comment_content`, `comment_karma`, `comment_approved`, `comment_agent`, `comment_type`, `comment_parent`, `user_id`) VALUES (1, 'A WordPress Commenter', 'wapuu@wordpress.example', 'https://wordpress.org/', '', '2026-11-20 10:00:00', '2026-11-20 10:00:00', 'Hi, this is a comment.', 0, '1', '', 'comment', 0, 0)
ok:1	SELECT * FROM wp_comments WHERE comment_ID = 1 LIMIT 1
ok:1	SELECT COUNT(*) FROM wp_comments WHERE comment_post_ID = 1 AND comment_approved = '1'
ok:1	UPDATE `wp_posts` SET `comment_count` = 1 WHERE `ID` = 1
ok:1	SELECT wp_comments.comment_ID FROM wp_comments WHERE ( comment_approved = '1' ) AND comment_post_ID = 1 AND comment_parent = 0 ORDER BY wp_comments.comment_date_gmt ASC, wp_comments.comment_ID ASC
ok:1	SELECT comment_approved, COUNT( * ) AS total FROM wp_comments GROUP BY comment_approved
ok:0	SELECT comment_id, meta_key, meta_value FROM wp_commentmeta WHERE comment_id IN (1) ORDER BY meta_id ASC
# -- the admin screens (wp_count_posts, the list tables, Site Health)
ok:1	SELECT post_status, COUNT( * ) AS num_posts FROM wp_posts WHERE post_type = 'post' GROUP BY post_status
ok:0	SELECT DISTINCT post_mime_type FROM wp_posts WHERE post_type = 'attachment'
ok:2	SELECT DISTINCT YEAR( post_date ) AS year, MONTH( post_date ) AS month FROM wp_posts WHERE post_type = 'post' ORDER BY post_date DESC
ok:12	SHOW TABLE STATUS
ok:1	SELECT CONVERT( 'Hello' USING utf8mb4 ) AS x_0
# -- one plugin activation (activate_plugin updates the list of active plugins)
ok:1	UPDATE `wp_options` SET `option_value` = 'a:1:{i:0;s:9:"hello.php";}' WHERE `option_name` = 'active_plugins'
ok:1	SELECT option_value FROM wp_options WHERE option_name = 'active_plugins' LIMIT 1
# -- export (wp export)
ok:3	SELECT ID FROM wp_posts WHERE wp_posts.post_type IN ('post','page') AND wp_posts.post_status != 'auto-draft'
ok:1	SELECT DISTINCT u.id, u.user_login, u.user_email, u.display_name FROM wp_users AS u INNER JOIN wp_posts AS p ON p.post_author = u.ID WHERE p.post_status != 'auto-draft'
# -- deleting (wp_delete_post, wp_delete_comment)
ok:1	DELETE FROM wp_postmeta WHERE meta_id IN( 2 )
ok:1	DELETE FROM `wp_comments` WHERE `comment_ID` = 1
ok:1	DELETE FROM `wp_posts` WHERE `ID` = 3
