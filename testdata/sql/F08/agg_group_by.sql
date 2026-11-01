CREATE DATABASE db1;
CREATE TABLE db1.t (id INT, name VARCHAR(20));
INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b');
SELECT COUNT(*) FROM db1.t;
SELECT name, COUNT(*) FROM db1.t GROUP BY name;
SELECT SUM(id), MIN(id), MAX(id) FROM db1.t;
