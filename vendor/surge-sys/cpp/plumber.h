#pragma once	// of course.
#include "src/common/SurgeSynthesizer.h"

// forgot the linking issue was in surge so this is all useless.
// but we're leaving it.
#if defined(_WIN32) || defined(__CYGWIN__)
	#define EXP __declspec(dllexport)
#else
	#define EXP __attribute__((visibility("default"))) __attribute__((used))
#endif

typedef SurgeSynthesizer::ID ID;

extern "C" {	// linkage?
	// note 1.
EXP	int getNumInputs(SurgeSynthesizer* surge);
EXP	int getNumOutputs(SurgeSynthesizer* surge);
EXP	int getBlockSize(SurgeSynthesizer* surge);
EXP	int getSynthSideId(const SurgeSynthesizer::ID* id);
#define CSUR const SurgeSynthesizer* surge
#define NSUR SurgeSynthesizer* surge
#define IAT1 const ID* index, char* text
#define IDPO const ID* index
EXP	bool fromSynthSideId			(CSUR, int i, ID* q);
EXP	void idForParameter			(CSUR, const Parameter* p, ID* q);
EXP	void getParameterDisplay		(CSUR, IAT1);
EXP	void getParameterDisplayAlt		(CSUR, IAT1);
EXP	void getParameterName			(CSUR, IAT1);
EXP	void getParameterNameExtendedByFXGroup	(CSUR, IAT1);
EXP	void getParameterAccessibleName		(CSUR, IAT1);
EXP	void getParameterMeta			(CSUR, IDPO, parametermeta* pm);
EXP	float getParameter01			(CSUR, IDPO);
EXP	bool setParameter01			(NSUR, IDPO, float value, bool external = false, bool force_integer = false);
EXP	float normalizedToValue			(CSUR, IDPO, float val);
EXP	float valueToNormalized			(CSUR, IDPO, float val);
EXP	void sendParameterAutomation		(NSUR, IDPO, float val);
#undef CSUR
#undef NSUR
#undef IAT1
#undef IDPO
EXP	void loadRaw				(const void* data, int size, bool preset = false);
}

/*
 * *note 1:
 * i forgot to define these [three] functions before and it still worked.
 * apparently you don't need a header file for ffi (in this case).
 * will still keep it, though. no reason to remove it.
 */
