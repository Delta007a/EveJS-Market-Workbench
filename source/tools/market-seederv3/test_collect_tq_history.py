import unittest
from collect_tq_history import known_unavailable


class CollectorTests(unittest.TestCase):
    def test_only_exact_official_nontradable_response_is_absent_history(self):
        url='https://esi.evetech.net/latest/markets/10000002/history/?type_id=93701'
        response=f'HTTP 400 from {url}; error-limit-remain=99; body={{"error":"Type not tradable on market!"}}'
        evidence=known_unavailable(RuntimeError(response),url)
        self.assertEqual(evidence['httpStatus'],400)
        self.assertEqual(len(evidence['bodySha256']),64)
        for invalid in (response.replace('400','500',1),response.replace('tradable','known'),response.replace(url,url+'0')):
            self.assertIsNone(known_unavailable(RuntimeError(invalid),url))


if __name__=='__main__':unittest.main()
