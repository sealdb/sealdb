CREATE DATABASE db1;
CREATE TABLE db1.t1 (id INT, name VARCHAR(20));
CREATE TABLE db1.t2 (id INT, tag VARCHAR(20));
INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b');
INSERT INTO db1.t2 VALUES (1, 'x'), (3, 'y');
SELECT t1.id, t1.name, t2.tag FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id;
SELECT t1.id, t2.tag FROM db1.t1 LEFT JOIN db1.t2 ON t1.id = t2.id;
SELECT t1.name, t2.tag FROM db1.t1 RIGHT JOIN db1.t2 ON t1.id = t2.id;
