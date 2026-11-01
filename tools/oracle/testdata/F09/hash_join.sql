SELECT t1.id, t1.name, t2.tag FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id;
SELECT t1.id, t2.tag FROM db1.t1 LEFT JOIN db1.t2 ON t1.id = t2.id;
SELECT t1.name, t2.tag FROM db1.t1 RIGHT JOIN db1.t2 ON t1.id = t2.id;
SELECT * FROM db1.t1 INNER JOIN db1.t2 ON t1.id = t2.id;
