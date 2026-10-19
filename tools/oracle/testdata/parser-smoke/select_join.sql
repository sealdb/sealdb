select t.id, s.c from t join s on t.a = s.a where t.b > 10 order by t.id limit 20;
select * from t left join s on t.id = s.id;
