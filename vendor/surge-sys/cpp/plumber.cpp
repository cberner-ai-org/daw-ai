#include "plumber.h"
#include <new>

typedef SurgeSynthesizer::ID ID;

extern "C" {
	// header functions that don't get exported by bindgen.
	// could be hard-coded but i'd rather have it be a bit more verbose in exchange for correctness.
EXP	int getNumInputs(SurgeSynthesizer* surge)	{ return surge->getNumInputs(); }
EXP	int getNumOutputs(SurgeSynthesizer* surge)	{ return surge->getNumOutputs(); }
EXP	int getBlockSize(SurgeSynthesizer* surge)	{ return surge->getBlockSize(); }
	// member functions that don't get exported by bindgen.
EXP	int getSynthSideId(const ID* id)		{ return id->getSynthSideId(); }
	// more header functions. note 1.
#define CSUR const SurgeSynthesizer* surge
#define NSUR SurgeSynthesizer* surge
#define IAT1 const ID* index, char* text
#define IAT2 *index, text
#define IDPO const ID* index
EXP	bool fromSynthSideId			(CSUR, int i, ID* q)		{ return surge->fromSynthSideId(i, *q); }
EXP	void idForParameter			(CSUR, const Parameter* p, ID* q)	{ ::new (q) ID(surge->idForParameter(p)); }
EXP	void getParameterDisplay		(CSUR, IAT1)			{ return surge->getParameterDisplay(IAT2); }
EXP	void getParameterDisplayAlt		(CSUR, IAT1)			{ return surge->getParameterDisplay(IAT2); }
EXP	void getParameterName			(CSUR, IAT1)			{ return surge->getParameterName(IAT2); }
EXP	void getParameterNameExtendedByFXGroup	(CSUR, IAT1)			{ return surge->getParameterNameExtendedByFXGroup(IAT2); }
EXP	void getParameterAccessibleName		(CSUR, IAT1)			{ return surge->getParameterAccessibleName(IAT2); }
EXP	void getParameterMeta			(CSUR, IDPO, parametermeta* pm)	{ return surge->getParameterMeta(*index, *pm); }
EXP	float getParameter01			(CSUR, IDPO)			{ return surge->getParameter01(*index); }
EXP	bool setParameter01			(NSUR, IDPO,
						float value,
						bool external,
						bool force_integer)
						{ return surge->setParameter01(
							*index, value, external, force_integer);
						} // this looks really bad.
EXP	float normalizedToValue(CSUR, IDPO, float val)		{ return surge->normalizedToValue(*index, val); }
EXP	float valueToNormalized(CSUR, IDPO, float val)		{ return surge->valueToNormalized(*index, val); }
EXP	void sendParameterAutomation(NSUR, IDPO, float val)	{ return surge->sendParameterAutomation(*index, val); }
#undef CSUR
#undef NSUR
#undef IAT1
#undef IAT2
#undef IDPO
}
// EXTERNAL FALSE, FORCE_INTEGER FALSE.

/*
 * note 1:
 * remember c doesn't know what references are!
 * also, standalone functions no longer take const (as outer definition).
 * this is why the inner arguments are const. shows no change to what would be self.
 * it also prevents having to pass mutable references from rust.
 */
