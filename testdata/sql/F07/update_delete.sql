CREATE DATABASE db1;
CREATE TABLE db1.t (id INT, name VARCHAR(20));
INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'c');
UPDATE db1.t SET name = 'x' WHERE id = 1;
DELETE FROM db1.t WHERE id = 2;
SELECT * FROM db1.t;
