skill_list = {}

if has embedding:
	ll := embedding_selector_top100()
        if highest score(ll) <= threshold_1:
		ll = {}
	else if has cheap llm:
		ll = cheap_llm_top5(ll)
       skill_list = ll
eles 
	ll = 分词策略_top20()
	if highest score(ll) <= thereshold_2:
		ll = {}
        skill_list = ll

x := max(5, min(20, 2%*total_skill_list))
if skill_list is not empty:
	skill_list = skill_list[:x]


	

